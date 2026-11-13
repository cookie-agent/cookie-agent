//! The Webhooks view (client protocol 1.96.0): every webhook with its
//! title, state, target, check, and its requests so far; the selected
//! webhook's URL to copy, details, actions (edit, enable or disable, send
//! a test event, rotate the URL or secret, reveal the secret, delete), and
//! its events, each opening to its request; a secret or URL the daemon
//! returned once; and the webhook form, which the ⋮ menu's "Add a webhook"
//! opens for the open session.

use cookie_agent_client_state::{
    cron::delivery_label,
    session_actions::permission_mode_label,
    webhook::{
        OutcomeKind, WebhookDraft, WebhookDraftTarget, check_label, counts_summary, detail_lines,
        event_outcome, event_time, events_newest_first, last_outcome, state_label, target_label,
        webhook_title,
    },
};
use cookie_agent_protocol::{
    PermissionMode, ProducerDeliveryMode, SessionWebhook, Webhook, WebhookEvent, WebhookId,
    WebhookRotate,
};
use leptos::prelude::*;

use super::{
    BUTTON, ERROR, NOTE, PRIMARY, ROW,
    cron::{LABEL, LINK, SELECT},
    empty_state, loading,
    manage::{CONFIRM, EDITOR_ACTIONS, EDITOR_PAGE, FIELD, STATE},
};
use crate::{
    cron::{model_pins, session_name, zone_at},
    icons::Glyph,
    modes::MODES,
    webhook::mark_title,
    workspace::{WebhookForm, Workspace},
};

/// A text area, as the form's long fields.
const AREA: &str = "min-h-[6rem] w-full resize-y rounded-lg bg-surface p-2 text-sm hover:shadow-[inset_0_0_0_1px_var(--color-faint)] focus:shadow-[inset_0_0_0_1px_var(--color-accent),0_0_0_3px_var(--color-accent-soft)] focus:outline-none shadow-[inset_0_0_0_1px_var(--color-line-2)]";

/// A code block: a URL, a secret, a request's body.
const CODE: &str =
    "min-w-0 flex-1 rounded-md bg-sunken px-3 py-2 font-mono text-xs break-all whitespace-pre-wrap";

/// What a confirmation under a webhook's actions asks.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Confirm {
    Delete,
    RotateUrl,
    RotateSecret,
}

pub fn webhooks_page(workspace: Workspace) -> impl IntoView {
    let confirm = RwSignal::new(None::<(WebhookId, Confirm)>);
    // The form renders once per opening: its fields keep what is typed.
    let form = Memo::new(move |_| workspace.webhook_form.get());
    move || {
        if let Some(form) = form.get() {
            return webhook_form(workspace, form).into_any();
        }
        let listing = match workspace.webhooks.get() {
            None => return loading("Reading webhooks…").into_any(),
            Some(Err(error)) => return view! { <p class=ERROR>{error}</p> }.into_any(),
            Some(Ok(listing)) => listing,
        };
        let rows = if listing.webhooks.is_empty() {
            empty_state(
                icondata::LuWebhook,
                "No webhooks. Add one here, or from a session's ⋮ menu.",
            )
            .into_any()
        } else {
            listing
                .webhooks
                .into_iter()
                .map(|webhook| webhook_row(workspace, webhook, confirm))
                .collect_view()
                .into_any()
        };
        view! {
            {super::view_header(
                "Webhooks",
                "URLs that turn each request they receive into a message in a session, or start one.",
                view! {
                    <button type="button" id="webhook-new" class=PRIMARY on:click=move |_| workspace.new_webhook()>
                        <Glyph icon=icondata::LuPlus />
                        "New webhook"
                    </button>
                },
            )}
            {(!listing.receiving).then(|| view! {
                <p class=ERROR id="webhooks-receiving-off">
                    "Receiving is off ([webhooks] enabled = false): every URL answers 404, and the webhooks stay."
                </p>
            })}
            {move || workspace.webhook_error.get().map(|error| view! { <p class=ERROR id="webhook-error">{error}</p> })}
            {move || workspace.webhook_revealed.get().map(|revealed| revealed_note(workspace, revealed))}
            {rows}
        }
        .into_any()
    }
}

/// A secret or URL the daemon returned once: shown with a copy button
/// until dismissed.
fn revealed_note(
    workspace: Workspace,
    revealed: crate::workspace::WebhookRevealed,
) -> impl IntoView {
    let url = revealed.new_url.then(|| {
        workspace
            .webhook_list()
            .into_iter()
            .find(|webhook| webhook.id == revealed.webhook)
            .map(|webhook| workspace.webhook_url(&webhook))
    });
    let heading = if revealed.secret.is_some() {
        format!(
            "{}: copy its secret now; it is shown here until you close this.",
            revealed.title
        )
    } else {
        format!("{}: its URL.", revealed.title)
    };
    view! {
        <div class="flex flex-col gap-2 rounded-xl bg-raised p-3 shadow-[0_0_0_1px_var(--color-accent),0_0_0_4px_var(--color-accent-soft)]" role="status" id="webhook-revealed">
            <p class="text-sm font-bold">{heading}</p>
            {url.flatten().map(|url| copy_line(workspace, "URL", url, "webhook-revealed-url"))}
            {revealed.secret.map(|secret| copy_line(workspace, "Secret", secret, "webhook-revealed-secret"))}
            <div>
                <button type="button" class=BUTTON id="webhook-revealed-close" on:click=move |_| workspace.webhook_revealed.set(None)>
                    "Done"
                </button>
            </div>
        </div>
    }
}

/// A value to copy: its label, the value, and a copy button.
fn copy_line(
    workspace: Workspace,
    label: &'static str,
    value: String,
    id: &'static str,
) -> impl IntoView {
    let copied = value.clone();
    view! {
        <div class="flex flex-col gap-1">
            <span class="text-xs font-bold text-muted">{label}</span>
            <div class="flex items-start gap-2">
                <code class=CODE id=id>{value}</code>
                <button
                    type="button"
                    class=BUTTON
                    title=format!("Copy the {}", label.to_lowercase())
                    aria-label=format!("Copy the {}", label.to_lowercase())
                    data-copy=id
                    on:click=move |_| {
                        let text = copied.clone();
                        leptos::task::spawn_local(async move {
                            let done = crate::browser::copy_text(&text).await;
                            workspace.status.set(if done {
                                format!("{label} copied.")
                            } else {
                                "This browser does not let the page copy; select the text instead.".to_owned()
                            });
                        });
                    }
                >
                    <Glyph icon=icondata::LuCopy />
                </button>
            </div>
        </div>
    }
}

/// A webhook's row: a click shows or hides its details.
fn webhook_row(
    workspace: Workspace,
    webhook: Webhook,
    confirm: RwSignal<Option<(WebhookId, Confirm)>>,
) -> impl IntoView {
    let id = webhook.id.clone();
    let selected = {
        let id = id.clone();
        Memo::new(move |_| {
            workspace
                .webhook_selected
                .with(|selected| selected.as_ref() == Some(&id))
        })
    };
    let title = webhook_title(&webhook);
    let target = workspace.projects.with_untracked(|projects| {
        target_label(&webhook.target, projects, |session| {
            workspace
                .sessions
                .with_untracked(|sessions| session_name(sessions, session))
        })
    });
    let state = state_label(&webhook);
    let check = check_label(webhook.verify.as_ref());
    let requests = counts_summary(&webhook.counts);
    let failing = last_outcome(&webhook) == Some(OutcomeKind::Failed);
    let enabled = webhook.enabled;
    let toggle = {
        let id = id.clone();
        move |_| {
            workspace.webhook_selected.update(|selected| {
                *selected = if selected.as_ref() == Some(&id) {
                    None
                } else {
                    Some(id.clone())
                };
            });
        }
    };
    let details = {
        let webhook = webhook.clone();
        move || {
            selected
                .get()
                .then(|| webhook_details(workspace, webhook.clone(), confirm))
        }
    };
    // A right-click offers the webhook's actions without opening it.
    let context = RwSignal::new(None);
    let context_items = {
        let webhook = webhook.clone();
        move || {
            use super::row_menu::MenuItem;
            let (edited, toggled, tested, asked) = (
                webhook.clone(),
                webhook.id.clone(),
                webhook.id.clone(),
                webhook.id.clone(),
            );
            let enabled = webhook.enabled;
            vec![
                MenuItem {
                    id: "edit",
                    label: "Edit",
                    icon: icondata::LuPencil,
                    act: Callback::new(move |()| workspace.edit_webhook(edited.clone())),
                    danger: false,
                },
                MenuItem {
                    id: if enabled { "disable" } else { "enable" },
                    label: if enabled { "Disable" } else { "Enable" },
                    icon: if enabled {
                        icondata::LuPause
                    } else {
                        icondata::LuPlay
                    },
                    act: Callback::new(move |()| {
                        workspace.set_webhook_enabled(toggled.clone(), !enabled)
                    }),
                    danger: false,
                },
                MenuItem {
                    id: "test",
                    label: "Send a test event",
                    icon: icondata::LuSend,
                    act: Callback::new(move |()| workspace.test_webhook(tested.clone())),
                    danger: false,
                },
                MenuItem {
                    id: "delete",
                    label: "Delete…",
                    icon: icondata::LuTrash2,
                    act: Callback::new(move |()| {
                        // The confirmation is in the webhook's details.
                        workspace.webhook_selected.set(Some(asked.clone()));
                        confirm.set(Some((asked.clone(), Confirm::Delete)));
                    }),
                    danger: true,
                },
            ]
        }
    };
    view! {
        {super::row_menu::context_menu(context, (), title.clone(), context_items)}
        <div
            on:contextmenu=super::row_menu::on_context(context)
            class=move || format!(
                "webhook flex flex-col gap-1 rounded-xl p-1 transition-shadow {}",
                if selected.get() {
                    "bg-raised shadow-[0_0_0_1px_var(--color-accent),0_0_0_4px_var(--color-accent-soft)]"
                } else {
                    "bg-raised shadow-[0_0_0_1px_var(--color-line),0_1px_2px_rgb(0_0_0/0.03)]"
                }
            )
            data-webhook=id.to_string()
            data-title=title.clone()
            data-enabled=enabled.to_string()
        >
            <div class="relative">
            <button
                type="button"
                role="switch"
                class="absolute top-1 right-1 z-[1] inline-flex h-11 w-11 items-center justify-center rounded-full lg:top-2.5 lg:right-2.5 lg:h-7"
                aria-checked=enabled.to_string()
                aria-label=if enabled { "Turn the webhook off" } else { "Turn the webhook on" }
                title=if enabled { "On: click to turn it off" } else { "Off: click to turn it on" }
                data-webhook-switch
                on:click={
                    let id = id.clone();
                    move |_| workspace.set_webhook_enabled(id.clone(), !enabled)
                }
            >
                <span class="effort-switch" class:effort-switch-on=enabled aria-hidden="true"></span>
            </button>
            <button
                type="button"
                class=format!("{ROW} pr-16")
                aria-expanded=move || selected.get().to_string()
                on:click=toggle
            >
                <span class="flex w-full items-center gap-2">
                    <Glyph
                        icon=icondata::LuWebhook
                        class=if enabled { "text-accent" } else { "opacity-50" }
                    />
                    <span class="min-w-0 flex-1 truncate text-sm font-bold" data-webhook-title>{title.clone()}</span>
                    <span class=STATE data-webhook-state>{state}</span>
                </span>
                <span class="pl-6 text-[0.8125rem] text-fg-2" data-webhook-target>{target}</span>
                <span class=format!("flex w-full flex-wrap gap-x-3 pl-6 {NOTE}")>
                    <span data-webhook-check>{check}</span>
                    <span data-webhook-requests class:text-bad=failing>{requests}</span>
                </span>
            </button>
            </div>
            {details}
        </div>
    }
}

/// The selected webhook: its URL, details, actions, and events, newest
/// first. The target and the session that created it link to their
/// sessions.
fn webhook_details(
    workspace: Workspace,
    webhook: Webhook,
    confirm: RwSignal<Option<(WebhookId, Confirm)>>,
) -> impl IntoView {
    let zone = zone_at(jiff::Timestamp::now());
    let url = workspace.webhook_url(&webhook);
    let private = workspace.webhook_urls_private();
    let title = |session| {
        workspace
            .sessions
            .with_untracked(|sessions| session_name(sessions, session))
    };
    let lines = workspace
        .projects
        .with_untracked(|projects| detail_lines(&webhook, &url, &zone, projects, title))
        .into_iter()
        // The URL has its own line, with a copy button.
        .filter(|detail| detail.label != "url")
        .map(|detail| {
            let value = match detail.session {
                Some(session) => view! {
                    <button
                        type="button"
                        class=LINK
                        data-webhook-session=session.to_string()
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
                <dt class="text-xs font-bold text-muted">{detail.label}</dt>
                <dd class="min-w-0 text-sm break-words" data-webhook-detail=detail.label>{value}</dd>
            }
        })
        .collect_view();
    let prompt = webhook.prompt.clone();
    let thread_prompt = match &webhook.target {
        cookie_agent_protocol::WebhookTarget::New(template) => template.thread_prompt.clone(),
        _ => None,
    };
    let history = if webhook.history.is_empty() {
        view! { <p class=NOTE>"No events yet. Send a test event to try it."</p> }.into_any()
    } else {
        events_newest_first(&webhook)
            .map(|event| event_line(workspace, webhook.id.clone(), event, &zone))
            .collect_view()
            .into_any()
    };
    let title = webhook_title(&webhook);
    let has_secret = webhook.verify.is_some();
    let edited = webhook.clone();
    let (toggled, enabled) = (webhook.id.clone(), webhook.enabled);
    let tested = webhook.id.clone();
    let id = webhook.id.clone();
    let ask = move |what: Confirm| {
        let id = id.clone();
        move |_| confirm.set(Some((id.clone(), what)))
    };
    let revealed = (webhook.id.clone(), title.clone());
    let confirming = {
        let id = webhook.id.clone();
        move || {
            confirm
                .get()
                .filter(|(asked, _)| *asked == id)
                .map(|(_, what)| what)
        }
    };
    let confirmed = (webhook.id.clone(), title.clone());
    view! {
        <div class="flex flex-col gap-3 px-3 pb-2" data-webhook-details>
            {copy_line(workspace, "URL", url, "webhook-url")}
            {private.then(|| view! {
                <p class=NOTE data-webhook-private>
                    "This URL works only where cookie's page can be reached. Set [public] url to the address a tunnel or Funnel gives the public port, so other services can reach it."
                </p>
            })}
            <dl class="grid grid-cols-[auto_1fr] items-baseline gap-x-3 gap-y-1">{lines}</dl>
            {thread_prompt.map(|text| view! {
                <div class="flex flex-col gap-1">
                    <span class="text-xs font-bold text-muted">"Thread prompt"</span>
                    <p class="rounded-md bg-sunken px-3 py-2 text-sm whitespace-pre-wrap break-words" data-webhook-thread-prompt>{text}</p>
                </div>
            })}
            {prompt.map(|text| view! {
                <div class="flex flex-col gap-1">
                    <span class="text-xs font-bold text-muted">"Prompt"</span>
                    <p class="rounded-md bg-sunken px-3 py-2 text-sm whitespace-pre-wrap break-words" data-webhook-prompt>{text}</p>
                </div>
            })}
            <div class="flex flex-wrap gap-2" data-webhook-actions>
                <button type="button" class=BUTTON data-webhook-action="edit" on:click=move |_| workspace.edit_webhook(edited.clone())>
                    <Glyph icon=icondata::LuPencil />
                    "Edit"
                </button>
                <button
                    type="button"
                    class=BUTTON
                    data-webhook-action=if enabled { "disable" } else { "enable" }
                    on:click=move |_| workspace.set_webhook_enabled(toggled.clone(), !enabled)
                >
                    <Glyph icon=if enabled { icondata::LuPause } else { icondata::LuPlay } />
                    {if enabled { "Disable" } else { "Enable" }}
                </button>
                <button type="button" class=BUTTON data-webhook-action="test" on:click=move |_| workspace.test_webhook(tested.clone())>
                    <Glyph icon=icondata::LuSend />
                    "Send a test event"
                </button>
                <button type="button" class=BUTTON data-webhook-action="rotate-url" on:click=ask(Confirm::RotateUrl)>
                    <Glyph icon=icondata::LuRefreshCw />
                    "New URL"
                </button>
                {has_secret.then(|| {
                    let revealed = revealed.clone();
                    view! {
                        <button type="button" class=BUTTON data-webhook-action="rotate-secret" on:click=ask(Confirm::RotateSecret)>
                            <Glyph icon=icondata::LuKeyRound />
                            "New secret"
                        </button>
                        <button
                            type="button"
                            class=BUTTON
                            data-webhook-action="reveal"
                            on:click=move |_| workspace.reveal_webhook_secret(revealed.0.clone(), revealed.1.clone())
                        >
                            <Glyph icon=icondata::LuEye />
                            "Show secret"
                        </button>
                    }
                })}
                <button type="button" class=BUTTON data-webhook-action="delete" on:click=ask(Confirm::Delete)>
                    <Glyph icon=icondata::LuTrash2 />
                    "Delete"
                </button>
            </div>
            {move || confirming().map(|what| {
                let (id, title) = confirmed.clone();
                let question = match what {
                    Confirm::Delete => format!("Delete {title}? Its URL stops working at once; the sessions it started stay."),
                    Confirm::RotateUrl => format!("Give {title} a new URL? The old one stops working at once, so update every sender."),
                    Confirm::RotateSecret => format!("Give {title} a new secret? Requests signed with the old one are refused from now on."),
                };
                let verb = match what {
                    Confirm::Delete => "Delete",
                    Confirm::RotateUrl => "New URL",
                    Confirm::RotateSecret => "New secret",
                };
                view! {
                    <div class=CONFIRM role="alertdialog" data-webhook-confirm>
                        <p class="text-sm">{question}</p>
                        <div class="flex gap-2">
                            <button type="button" id="webhook-confirm" class=PRIMARY on:click=move |_| {
                                confirm.set(None);
                                match what {
                                    Confirm::Delete => workspace.delete_webhook(id.clone(), title.clone()),
                                    Confirm::RotateUrl => workspace.rotate_webhook(id.clone(), WebhookRotate::Url),
                                    Confirm::RotateSecret => workspace.rotate_webhook(id.clone(), WebhookRotate::Secret),
                                }
                            }>
                                {verb}
                            </button>
                            <button type="button" class=BUTTON on:click=move |_| confirm.set(None)>
                                "Keep it"
                            </button>
                        </div>
                    </div>
                }
            })}
            <div class="flex flex-col gap-1" data-webhook-history>
                <span class="text-xs font-bold text-muted">"Events"</span>
                {history}
            </div>
        </div>
    }
}

/// One event: when, what it did, its thread, and the session it went to;
/// a press opens its request.
fn event_line(
    workspace: Workspace,
    webhook: WebhookId,
    event: &WebhookEvent,
    zone: &jiff::tz::TimeZone,
) -> impl IntoView {
    let when = event_time(event, zone);
    let outcome = event_outcome(event);
    let failed = matches!(
        cookie_agent_client_state::webhook::outcome_kind(event),
        OutcomeKind::Failed
    );
    let thread = event.thread_key.clone();
    let session = event.session_id;
    let id = event.id.clone();
    let open = {
        let (webhook, id) = (webhook.clone(), id.clone());
        Memo::new(move |_| {
            workspace.webhook_event.with(|open| {
                open.as_ref()
                    .is_some_and(|open| open.webhook == webhook && open.event == id)
            })
        })
    };
    let toggle = {
        let (webhook, id) = (webhook.clone(), id.clone());
        move |_| workspace.toggle_webhook_event(webhook.clone(), id.clone())
    };
    view! {
        <div class="flex flex-col" data-webhook-event=id.clone()>
            <div class="flex min-h-11 flex-wrap items-center gap-x-3 text-sm">
                <button
                    type="button"
                    class="inline-flex min-h-11 min-w-0 flex-1 items-center gap-3 rounded-lg px-1 text-left hover:bg-hover"
                    aria-expanded=move || open.get().to_string()
                    title="Show the request"
                    on:click=toggle
                >
                    <span class="inline-flex transition-transform" class:rotate-90=move || open.get()>
                        <Glyph icon=icondata::LuChevronRight />
                    </span>
                    <span class="font-mono text-xs">{when}</span>
                    <span class="min-w-0 flex-1 break-words" class:text-bad=failed data-webhook-event-outcome>{outcome}</span>
                    {thread.map(|key| view! { <span class=NOTE>{format!("thread {key}")}</span> })}
                </button>
                {session.map(|session| view! {
                    <button
                        type="button"
                        class=BUTTON
                        data-webhook-event-session=session.to_string()
                        on:click=move |_| {
                            workspace.open_view(crate::layout::View::Chat, None);
                            workspace.open_session(session);
                        }
                    >
                        <Glyph icon=icondata::LuExternalLink />
                        "Open session"
                    </button>
                })}
            </div>
            {move || open.get().then(|| event_request(workspace))}
        </div>
    }
}

/// The open event's request: its method, headers, and body.
fn event_request(workspace: Workspace) -> impl IntoView {
    move || {
        let request = workspace
            .webhook_event
            .with(|open| open.as_ref().and_then(|open| open.request.clone()));
        match request {
            None => loading("Reading the request…").into_any(),
            Some(Err(error)) => view! { <p class=ERROR>{error}</p> }.into_any(),
            Some(Ok(request)) => {
                let headers = request
                    .headers
                    .iter()
                    .map(|header| format!("{}: {}", header.name, header.value))
                    .collect::<Vec<_>>()
                    .join("\n");
                let body = match (request.body_text, request.body_base64) {
                    (Some(text), _) => text,
                    (None, Some(_)) => format!("({} bytes of binary content)", request.event.bytes),
                    (None, None) => "(no body)".to_owned(),
                };
                view! {
                    <div class="flex flex-col gap-2 py-2 pl-7" data-webhook-request>
                        <code class=CODE>{format!("{}\n{headers}", request.method)}</code>
                        <code class=format!("{CODE} max-h-80 overflow-y-auto") data-webhook-body>{body}</code>
                    </div>
                }
                .into_any()
            }
        }
    }
}

/// The webhook form: a new webhook, or an edit of one.
#[allow(clippy::too_many_lines, reason = "one form, field by field")]
fn webhook_form(workspace: Workspace, form: WebhookForm) -> impl IntoView {
    let WebhookForm {
        editing,
        draft,
        session,
        template,
    } = form;
    let name = RwSignal::new(draft.name.clone());
    let prompt = RwSignal::new(draft.prompt.clone());
    let thread_key = RwSignal::new(draft.thread_key.clone());
    let thread_prompt = RwSignal::new(draft.thread_prompt.clone());
    let delivery = RwSignal::new(draft.delivery);
    let verify = RwSignal::new(draft.verify.clone());
    let secret = RwSignal::new(String::new());
    let filter = RwSignal::new(draft.filter.clone());
    let headers = RwSignal::new(draft.headers.clone());
    let max_per_hour = RwSignal::new(draft.max_per_hour.clone());
    let max_concurrent = RwSignal::new(draft.max_concurrent.clone());
    let max_failures = RwSignal::new(draft.max_consecutive_failures.clone());
    let new = RwSignal::new(draft.target.is_new());
    let agent = RwSignal::new(if draft.agent.is_empty() {
        template.agent.to_string()
    } else {
        draft.agent.clone()
    });
    let model = RwSignal::new(draft.model.clone());
    let mode = RwSignal::new(draft.permission_mode);
    let creating = editing.is_none();
    let threaded = move || new.get() && !thread_key.with(|key| key.trim().is_empty());
    let target_note = editing.as_ref().map(|webhook| {
        workspace.projects.with_untracked(|projects| {
            target_label(&webhook.target, projects, |id| {
                workspace
                    .sessions
                    .with_untracked(|sessions| session_name(sessions, id))
            })
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
            let mut draft = WebhookDraft {
                name: name.get_untracked(),
                prompt: prompt.get_untracked(),
                thread_key: thread_key.get_untracked(),
                thread_prompt: thread_prompt.get_untracked(),
                delivery: delivery.get_untracked(),
                verify: verify.get_untracked(),
                secret: secret.get_untracked(),
                filter: filter.get_untracked(),
                headers: headers.get_untracked(),
                max_per_hour: max_per_hour.get_untracked(),
                max_concurrent: max_concurrent.get_untracked(),
                max_consecutive_failures: max_failures.get_untracked(),
                permission_mode: mode.get_untracked(),
                model: model.get_untracked(),
                agent: agent.get_untracked(),
                ..draft.clone()
            };
            if creating {
                draft.target = match (new.get_untracked(), session) {
                    (false, Some((session, subagent))) => {
                        WebhookDraftTarget::Session { session, subagent }
                    }
                    _ => WebhookDraftTarget::New {
                        template: template.clone(),
                    },
                };
            }
            draft
        }
    };
    let heading = match &editing {
        Some(webhook) => format!("Edit {}", webhook_title(webhook)),
        None => "Add a webhook".to_owned(),
    };
    let text_field = move |id: &'static str,
                           label: &'static str,
                           value: RwSignal<String>,
                           placeholder: &'static str| {
        view! {
            <label class=LABEL>
                <span class="font-bold">{label}</span>
                <input
                    type="text"
                    id=id
                    class=FIELD
                    placeholder=placeholder
                    prop:value=move || value.get()
                    on:input=move |event| value.set(event_target_value(&event))
                />
            </label>
        }
    };
    view! {
        <div class=EDITOR_PAGE id="webhook-form">
            <h2 class="text-sm font-bold">{heading}</h2>
            {text_field("webhook-name", "Name", name, "optional; lists show its ID without one")}
            {match target_note {
                Some(target) => view! {
                    <p class="text-sm" id="webhook-target">{format!("Target: {target}")}</p>
                }
                .into_any(),
                None => view! {
                    <div class="flex flex-col gap-1" role="radiogroup" aria-label="Target">
                        <span class="text-sm font-bold">"Target"</span>
                        <div class="flex flex-wrap gap-2">
                            {this_session.clone().map(|label| view! {
                                <button
                                    type="button"
                                    id="webhook-target-session"
                                    role="radio"
                                    class=move || format!("{BUTTON} {}", if new.get() { "" } else { "bg-sunken" })
                                    aria-checked=move || (!new.get()).to_string()
                                    on:click=move |_| new.set(false)
                                >
                                    {label}
                                </button>
                            })}
                            <button
                                type="button"
                                id="webhook-target-new"
                                role="radio"
                                class=move || format!("{BUTTON} {}", if new.get() { "bg-sunken" } else { "" })
                                aria-checked=move || new.get().to_string()
                                on:click=move |_| new.set(true)
                            >
                                "New sessions"
                            </button>
                        </div>
                    </div>
                }
                .into_any(),
            }}
            <Show when=move || new.get()>
                <div class="grid gap-2 sm:grid-cols-2">
                    <label class=LABEL>
                        <span class="font-bold">"Agent"</span>
                        <select id="webhook-agent" class=SELECT on:change=move |event| agent.set(event_target_value(&event))>
                            {agents.clone().into_iter().map(|id| {
                                let (chosen, value) = (id.clone(), id.clone());
                                view! { <option value=value selected=move || agent.get() == chosen>{id}</option> }
                            }).collect_view()}
                        </select>
                    </label>
                    <label class=LABEL>
                        <span class="font-bold">"Model"</span>
                        <select id="webhook-model" class=SELECT on:change=move |event| model.set(event_target_value(&event))>
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
                            id="webhook-mode"
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
                    {text_field("webhook-max-concurrent", "Sessions at once", max_concurrent, "1; 0 or less for no limit")}
                </div>
                {text_field("webhook-thread-key", "Thread key", thread_key, "json:/issue/number  or  header:X-Thread")}
                <p class=NOTE>"Events with the same key go into one session, which the first of them starts. Without a key, each event starts its own session."</p>
                <label class=LABEL>
                    <span class="font-bold">"Thread prompt"</span>
                    <textarea
                        id="webhook-thread-prompt"
                        class=AREA
                        placeholder="optional; sent as your message when a session starts, and again after each compaction"
                        prop:value=move || thread_prompt.get()
                        on:input=move |event| thread_prompt.set(event_target_value(&event))
                    ></textarea>
                </label>
            </Show>
            <label class=LABEL>
                <span class="font-bold">"Prompt"</span>
                <textarea
                    id="webhook-prompt"
                    class=AREA
                    placeholder="optional; sent with every event, inside its webhook_event block"
                    prop:value=move || prompt.get()
                    on:input=move |event| prompt.set(event_target_value(&event))
                ></textarea>
            </label>
            <Show when=move || !new.get() || threaded()>
                <label class=LABEL>
                    <span class="font-bold">"Delivery"</span>
                    <select
                        id="webhook-delivery"
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
            <div class="grid gap-2 sm:grid-cols-2">
                {text_field("webhook-verify", "Check", verify, "github, standard, bearer, or hmac_sha256 header=…")}
                {text_field("webhook-secret", "Secret", secret, "blank: the daemon makes one, or keeps it")}
            </div>
            <p class=NOTE>"Without a check, the URL is the only key. hmac_sha256 also takes encoding=hex|base64 and prefix=…"</p>
            <label class=LABEL>
                <span class="font-bold">"Filter"</span>
                <textarea
                    id="webhook-filter"
                    class=format!("{AREA} min-h-[4rem] font-mono")
                    placeholder="one condition per line, such as header:X-GitHub-Event=issues,pull_request or json:/action=opened"
                    prop:value=move || filter.get()
                    on:input=move |event| filter.set(event_target_value(&event))
                ></textarea>
            </label>
            <div class="grid gap-2 sm:grid-cols-3">
                {text_field("webhook-headers", "Headers shown", headers, "X-GitHub-Event, …")}
                {text_field("webhook-max-per-hour", "Events per hour", max_per_hour, "the daemon's default")}
                {text_field("webhook-max-failures", "Failures before off", max_failures, "the daemon's default")}
            </div>
            {move || workspace.webhook_error.get().map(|error| view! { <p class=ERROR id="webhook-error">{error}</p> })}
            <div class=EDITOR_ACTIONS>
                <button type="button" id="webhook-save" class=PRIMARY on:click=move |_| workspace.save_webhook(build())>
                    {if creating { "Add" } else { "Save" }}
                </button>
                <button type="button" id="webhook-cancel" class=BUTTON on:click=move |_| workspace.close_webhook_form()>
                    "Cancel"
                </button>
            </div>
        </div>
    }
}

/// A webhook session's mark in the session list: a click opens the
/// webhook whose event started the session, while a press that moves
/// drags the row like the rest of it.
pub fn webhook_mark(workspace: Workspace, session: SessionWebhook) -> impl IntoView {
    let id = session.webhook_id.clone();
    let title = {
        let session = session.clone();
        move || {
            workspace.webhooks.with(|webhooks| {
                let list = webhooks
                    .as_ref()
                    .and_then(|webhooks| webhooks.as_ref().ok())
                    .map(|webhooks| webhooks.webhooks.as_slice())
                    .unwrap_or_default();
                mark_title(&session, list)
            })
        }
    };
    view! {
        <span
            class="hit-area relative inline-flex shrink-0 text-accent opacity-80 hover:opacity-100"
            role="link"
            tabindex="0"
            aria-label=title.clone()
            title=title
            data-session-webhook=session.webhook_id.to_string()
            on:click=move |event: leptos::ev::MouseEvent| {
                event.stop_propagation();
                workspace.open_webhook(id.clone());
            }
        >
            <Glyph icon=icondata::LuWebhook class="text-xs" />
        </span>
    }
}
