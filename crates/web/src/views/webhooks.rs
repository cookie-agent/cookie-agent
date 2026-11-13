//! The Webhooks view (client protocol 1.97.0,
//! `docs/site/specs/webhooks-view.md`): every webhook as a row with its
//! target in words and how it is doing (H1); an open webhook as a page of
//! its own (H2), with its setup, its events (H3), and its rules, and the
//! ready card a new one opens on (H5); the form is
//! [`super::webhook_form`].

use cookie_agent_client_state::{
    format::relative_time,
    memory::exact_time,
    webhook::{
        OutcomeKind, check_label, detail_lines, event_outcome, event_time, events_newest_first,
        outcome_kind, webhook_title,
    },
    webhook_view::{
        Counter, Health, Preset, Tone, counters, health, preset_of, row_tags, target_words,
        what_happens,
    },
};
use cookie_agent_protocol::{
    SessionId, SessionWebhook, Webhook, WebhookEvent, WebhookId, WebhookRotate, WebhookTarget,
    WebhookVerify,
};
use jiff::Timestamp;
use leptos::prelude::*;

use super::{
    BUTTON, ERROR, ICON_BUTTON, NOTE, PRIMARY, WARNING, empty_state, loading,
    manage::CONFIRM,
    row_menu::{MenuItem, context_menu, row_menu, wants_context_menu},
};
use crate::{
    cron::{session_name, zone_at},
    icons::Glyph,
    layout::{SettingsSection, View},
    webhook::mark_title,
    workspace::{WebhookTab, Workspace},
};

/// A code block: a URL, a secret, a request's body.
const CODE: &str =
    "min-w-0 flex-1 rounded-lg bg-sunken px-3 py-2 font-mono text-xs break-all whitespace-pre-wrap";

/// A small tag after a name.
const TAG: &str = "inline-flex h-5 shrink-0 items-center rounded-md bg-sunken px-1.5 text-[0.6875rem] font-semibold text-muted";

/// A group's heading on a webhook's page.
const GROUP: &str = "pt-3 text-xs font-semibold tracking-wide text-muted uppercase";

/// The ⋯ of a row, kept clear of the row's open button under it.
const MORE: &str = "relative z-[1] inline-flex min-h-11 min-w-11 shrink-0 items-center justify-center rounded-lg text-muted transition-colors hover:bg-hover hover:text-fg active:bg-press lg:min-h-9 lg:min-w-9";

/// What a confirmation on a webhook's page asks.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Confirm {
    Delete,
    RotateUrl,
    RotateSecret,
}

pub fn webhooks_page(workspace: Workspace) -> impl IntoView {
    let confirm = RwSignal::new(None::<(WebhookId, Confirm)>);
    let menu = RwSignal::new(None::<WebhookId>);
    let context = RwSignal::new(None::<(WebhookId, f64, f64)>);
    // The form renders once per opening: its fields keep what is typed.
    let form = Memo::new(move |_| workspace.webhook_form.get());
    move || {
        if let Some(form) = form.get() {
            return super::webhook_form::webhook_form(workspace, form).into_any();
        }
        let listing = match workspace.webhooks.get() {
            None => return loading("Reading webhooks…").into_any(),
            Some(Err(error)) => return view! { <p class=ERROR>{error}</p> }.into_any(),
            Some(Ok(listing)) => listing,
        };
        let receiving = listing.receiving;
        if let Some(webhook) = workspace.webhook_selected.get().and_then(|id| {
            listing
                .webhooks
                .iter()
                .find(|webhook| webhook.id == id)
                .cloned()
        }) {
            return webhook_page(workspace, webhook, receiving, confirm, menu).into_any();
        }
        let rows = if listing.webhooks.is_empty() {
            empty_state(
                icondata::LuWebhook,
                "No webhooks. Add one here, or from a session's ⋮ menu.",
            )
            .into_any()
        } else {
            view! {
                <div class="flex flex-col gap-2" data-webhook-list>
                    {listing
                        .webhooks
                        .into_iter()
                        .map(|webhook| webhook_row(workspace, webhook, confirm, menu, context))
                        .collect_view()}
                </div>
            }
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
            {notices(workspace, receiving)}
            {rows}
        }
        .into_any()
    }
}

/// What the list and a page say above themselves: receiving off, the
/// local-URL callout (W5), and the last error.
fn notices(workspace: Workspace, receiving: bool) -> impl IntoView {
    let private = workspace.webhook_urls_private();
    view! {
        {(!receiving).then(|| view! {
            <p class=ERROR id="webhooks-receiving-off">
                "Receiving is off ([webhooks] enabled = false): every URL answers 404, and the webhooks stay."
            </p>
        })}
        {private.then(|| view! {
            <div class=WARNING role="note" id="webhooks-private">
                <Glyph icon=icondata::LuTriangleAlert class="mt-0.5 shrink-0 text-warn" />
                <p class="min-w-0">
                    <b>"These URLs work only where this page can be reached. "</b>
                    "Services on the internet cannot send to them until "
                    <button
                        type="button"
                        class="hit-area font-semibold text-accent underline-offset-2 hover:underline"
                        id="webhooks-public-url"
                        on:click=move |_| workspace.open_view(View::Settings, Some(SettingsSection::Configuration))
                    >
                        "[public] url"
                    </button>
                    " is set to the address a tunnel or Funnel gives the public port."
                </p>
            </div>
        })}
        {move || workspace.webhook_error.get().map(|error| view! { <p class=ERROR id="webhook-error">{error}</p> })}
    }
}

/// The icon of the preset a webhook's check matches.
fn preset_icon(preset: Preset) -> icondata::Icon {
    match preset {
        Preset::Github => icondata::LuGithub,
        Preset::Standard => icondata::LuShieldCheck,
        Preset::Bearer => icondata::LuKeyRound,
        Preset::Anything => icondata::LuBraces,
    }
}

/// The icon a webhook is drawn with.
fn webhook_icon(verify: Option<&WebhookVerify>) -> icondata::Icon {
    preset_icon(preset_of(verify))
}

/// A session's name, as the client knows it.
fn name_of(workspace: Workspace) -> impl Fn(SessionId) -> Option<String> + Copy {
    move |session| {
        workspace
            .sessions
            .with_untracked(|sessions| session_name(sessions, session))
    }
}

/// A button that opens session `session`, named `name`.
fn session_link(workspace: Workspace, session: SessionId, name: String) -> impl IntoView {
    view! {
        <button
            type="button"
            class="hit-area relative z-[1] inline font-medium text-accent underline-offset-2 hover:underline"
            data-webhook-session=session.to_string()
            title="Open this session"
            on:click=move |event: leptos::ev::MouseEvent| {
                event.stop_propagation();
                workspace.open_view(View::Chat, None);
                workspace.open_session(session);
            }
        >
            {name}
        </button>
    }
}

/// Where a webhook's events go, in words, the session linked (H1).
fn target_line(workspace: Workspace, webhook: &Webhook) -> impl IntoView + use<> {
    let words = workspace
        .projects
        .with_untracked(|projects| target_words(webhook, projects, name_of(workspace)));
    view! {
        // Inline text, so a phone wraps it as a sentence; one line, cut
        // short, in a wide window.
        <span class="block min-w-0 text-[0.8125rem] text-fg-2 sm:truncate" data-webhook-target>
            <span class="text-muted" aria-hidden="true">"→ "</span>
            {words.session.map(|(session, name)| view! {
                {session_link(workspace, session, name)}
                <span class="text-muted">" · "</span>
            })}
            {words.rest}
        </span>
    }
}

/// The dot of a tone.
fn dot(tone: Tone) -> &'static str {
    match tone {
        Tone::Good => "bg-ok",
        Tone::Warn => "bg-warn",
        Tone::Bad => "bg-bad",
        Tone::Quiet => "bg-faint",
    }
}

/// A row's health line: the dot, the last event and when, and the last
/// refusal with its reason (H1, W2, W3).
fn health_line(health: Health, now: Timestamp) -> impl IntoView {
    let Health {
        tone,
        last_event,
        refusal,
    } = health;
    let tone_class = if tone == Tone::Bad {
        "text-bad"
    } else {
        "text-muted"
    };
    view! {
        <span class="flex min-w-0 flex-wrap items-center gap-x-3 gap-y-0.5 text-xs" data-webhook-health data-tone=format!("{tone:?}").to_lowercase()>
            <span class=format!("inline-flex min-w-0 items-center gap-1.5 {tone_class}")>
                <span class=format!("h-2 w-2 shrink-0 rounded-full {}", dot(tone)) aria-hidden="true"></span>
                {match &last_event {
                    Some((at, outcome)) => view! {
                        <span data-webhook-last title=exact_time(*at)>
                            {format!("Last event {} · {outcome}", relative_time(now, *at))}
                        </span>
                    }
                    .into_any(),
                    None => view! { <span data-webhook-last>"No events yet"</span> }.into_any(),
                }}
            </span>
            {refusal.map(|refusal| view! {
                <span class="min-w-0 text-warn" data-webhook-refusal title=exact_time(refusal.at)>
                    {format!(
                        "{} {} {}: {}",
                        refusal.count,
                        refusal.what,
                        relative_time(now, refusal.at),
                        refusal.reason
                    )}
                </span>
            })}
        </span>
    }
}

/// A webhook's on/off switch.
fn switch(workspace: Workspace, webhook: &Webhook) -> impl IntoView + use<> {
    let (id, enabled) = (webhook.id.clone(), webhook.enabled);
    view! {
        <button
            type="button"
            role="switch"
            class="relative z-[1] inline-flex min-h-11 min-w-11 shrink-0 items-center justify-center rounded-full lg:min-h-9"
            aria-checked=enabled.to_string()
            aria-label=if enabled { "Turn the webhook off" } else { "Turn the webhook on" }
            title=if enabled { "On: click to turn it off" } else { "Off: click to turn it on" }
            data-webhook-switch
            on:click=move |event: leptos::ev::MouseEvent| {
                event.stop_propagation();
                workspace.set_webhook_enabled(id.clone(), !enabled);
            }
        >
            <span class="effort-switch" class:effort-switch-on=enabled aria-hidden="true"></span>
        </button>
    }
}

/// A webhook's actions, for its ⋯ and a right-click: Edit, a test event,
/// New URL, New secret (with a check), and Delete; the last three ask on
/// its page first.
fn menu_items(
    workspace: Workspace,
    webhook: &Webhook,
    confirm: RwSignal<Option<(WebhookId, Confirm)>>,
) -> impl Fn() -> Vec<MenuItem> + Send + Sync + Clone + 'static + use<> {
    let webhook = webhook.clone();
    move || {
        let ask = |what: Confirm| {
            let id = webhook.id.clone();
            Callback::new(move |()| {
                // The confirmation is on the webhook's page.
                if workspace.webhook_selected.get_untracked() != Some(id.clone()) {
                    workspace.show_webhook(id.clone());
                }
                confirm.set(Some((id.clone(), what)));
            })
        };
        let (edited, tested) = (webhook.clone(), webhook.id.clone());
        let mut items = vec![
            MenuItem {
                id: "edit",
                label: "Edit",
                icon: icondata::LuPencil,
                act: Callback::new(move |()| workspace.edit_webhook(edited.clone())),
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
                id: "rotate-url",
                label: "New URL…",
                icon: icondata::LuRefreshCw,
                act: ask(Confirm::RotateUrl),
                danger: false,
            },
        ];
        if webhook.verify.is_some() {
            items.push(MenuItem {
                id: "rotate-secret",
                label: "New secret…",
                icon: icondata::LuKeyRound,
                act: ask(Confirm::RotateSecret),
                danger: false,
            });
        }
        items.push(MenuItem {
            id: "delete",
            label: "Delete…",
            icon: icondata::LuTrash2,
            act: ask(Confirm::Delete),
            danger: true,
        });
        items
    }
}

/// A webhook's row (H1): a press opens its page.
fn webhook_row(
    workspace: Workspace,
    webhook: Webhook,
    confirm: RwSignal<Option<(WebhookId, Confirm)>>,
    menu: RwSignal<Option<WebhookId>>,
    context: RwSignal<Option<(WebhookId, f64, f64)>>,
) -> impl IntoView {
    let now = Timestamp::now();
    let id = webhook.id.clone();
    let title = webhook_title(&webhook);
    let enabled = webhook.enabled;
    let tags = row_tags(&webhook);
    let items = menu_items(workspace, &webhook, confirm);
    let open = {
        let id = id.clone();
        move |_| workspace.show_webhook(id.clone())
    };
    let opened_context = {
        let id = id.clone();
        move |event: leptos::ev::MouseEvent| {
            if !crate::browser::from_touch(&event) && wants_context_menu(&event) {
                event.prevent_default();
                context.set(Some((
                    id.clone(),
                    f64::from(event.client_x()),
                    f64::from(event.client_y()),
                )));
            }
        }
    };
    view! {
        {context_menu(context, id.clone(), title.clone(), items.clone())}
        <div
            class="webhook relative flex items-start gap-3 rounded-xl bg-raised py-2 pr-1 pl-3 shadow-[0_0_0_1px_var(--color-line),0_1px_2px_rgb(0_0_0/0.03)] transition-colors hover:bg-hover"
            class:opacity-70=!enabled
            on:contextmenu=opened_context
            data-webhook=id.to_string()
            data-title=title.clone()
            data-enabled=enabled.to_string()
        >
            <button
                type="button"
                class="absolute inset-0 rounded-xl"
                aria-label=format!("Open {title}")
                data-webhook-open
                on:click=open
            ></button>
            <span class="mt-1.5 inline-flex h-8 w-8 shrink-0 items-center justify-center rounded-lg bg-sunken text-fg-2">
                <Glyph icon=webhook_icon(webhook.verify.as_ref()) />
            </span>
            <span class="flex min-w-0 flex-1 flex-col gap-0.5 py-1">
                <span class="flex min-w-0 items-center gap-2">
                    <span class="min-w-0 truncate text-sm font-semibold" data-webhook-title>{title.clone()}</span>
                    {tags.into_iter().map(|tag| view! { <span class=TAG data-webhook-tag>{tag}</span> }).collect_view()}
                </span>
                {target_line(workspace, &webhook)}
                {health_line(health(&webhook), now)}
            </span>
            {switch(workspace, &webhook)}
            {row_menu(menu, id.clone(), id.to_string(), title.clone(), MORE, items)}
        </div>
    }
}

/// An open webhook (H2): its head, its actions, a ready card when it is
/// new (H5), and its three tabs.
fn webhook_page(
    workspace: Workspace,
    webhook: Webhook,
    receiving: bool,
    confirm: RwSignal<Option<(WebhookId, Confirm)>>,
    menu: RwSignal<Option<WebhookId>>,
) -> impl IntoView {
    let title = webhook_title(&webhook);
    let url = workspace.webhook_url(&webhook);
    let subtitle = {
        let words = workspace
            .projects
            .with_untracked(|projects| target_words(&webhook, projects, name_of(workspace)));
        let target = match words.session {
            Some((_, name)) => format!("into {name}"),
            None => words.rest,
        };
        format!("{} · {target}", check_label(webhook.verify.as_ref()))
    };
    let items = menu_items(workspace, &webhook, confirm);
    let created = {
        let id = webhook.id.clone();
        move || {
            workspace.webhook_revealed.with(|shown| {
                shown
                    .as_ref()
                    .is_some_and(|shown| shown.webhook == id && shown.created)
            })
        }
    };
    let ready = {
        let (webhook, url) = (webhook.clone(), url.clone());
        move || created().then(|| ready_card(workspace, &webhook, url.clone()))
    };
    let tested = webhook.id.clone();
    let copied_url = url.clone();
    let events = webhook.history.len();
    let tab = workspace.webhook_tab;
    let tab_button = move |which: WebhookTab, label: &'static str, count: Option<usize>| {
        view! {
            <button
                type="button"
                role="tab"
                class="inline-flex min-h-11 shrink-0 items-center gap-1.5 border-b-2 px-2 text-[0.8125rem] whitespace-nowrap transition-colors aria-selected:border-accent aria-selected:font-semibold aria-selected:text-fg border-transparent text-muted hover:text-fg lg:min-h-9"
                aria-selected=move || (tab.get() == which).to_string()
                data-webhook-tab=label
                on:click=move |_| tab.set(which)
            >
                {label}
                {count.filter(|count| *count > 0).map(|count| view! {
                    <span class="rounded-md bg-sunken px-1 text-[0.6875rem] font-semibold text-muted">{count}</span>
                })}
            </button>
        }
    };
    let body = {
        let webhook = webhook.clone();
        let url = url.clone();
        move || match tab.get() {
            WebhookTab::Setup => setup_tab(workspace, webhook.clone(), url.clone()).into_any(),
            WebhookTab::Events => events_tab(workspace, &webhook).into_any(),
            WebhookTab::Rules => rules_tab(workspace, &webhook, &url).into_any(),
        }
    };
    view! {
        <div class="flex flex-col gap-3" id="webhook-page" data-webhook-page=webhook.id.to_string()>
            <div class="flex items-center gap-3">
                <span class="inline-flex h-10 w-10 shrink-0 items-center justify-center rounded-xl bg-sunken text-fg-2">
                    <Glyph icon=webhook_icon(webhook.verify.as_ref()) class="text-lg" />
                </span>
                <div class="min-w-0 flex-1">
                    <h2 class="truncate text-xl font-semibold tracking-tight" id="webhook-page-title">{title.clone()}</h2>
                    <p class="truncate text-[0.8125rem] text-muted" data-webhook-subtitle>{subtitle}</p>
                </div>
                {switch(workspace, &webhook)}
                {row_menu(menu, webhook.id.clone(), webhook.id.to_string(), title.clone(), ICON_BUTTON, items)}
            </div>
            // Two big actions, side by side on a phone (H6).
            <div class="flex gap-2" data-webhook-actions>
                <button
                    type="button"
                    class=format!("{BUTTON} flex-1 sm:flex-none")
                    data-webhook-action="test"
                    on:click=move |_| workspace.test_webhook(tested.clone())
                >
                    <Glyph icon=icondata::LuSend />
                    "Send a test event"
                </button>
                <button
                    type="button"
                    class=format!("{BUTTON} flex-1 sm:flex-none")
                    data-webhook-action="copy-url"
                    on:click=move |_| copy(workspace, "URL", copied_url.clone())
                >
                    <Glyph icon=icondata::LuCopy />
                    "Copy URL"
                </button>
            </div>
            {notices(workspace, receiving)}
            {confirmation(workspace, &webhook, confirm)}
            {ready}
            <div class="flex gap-1 overflow-x-auto border-b border-line" role="tablist" aria-label="Webhook">
                {tab_button(WebhookTab::Setup, "Setup", None)}
                {tab_button(WebhookTab::Events, "Events", Some(events))}
                {tab_button(WebhookTab::Rules, "Rules & limits", None)}
            </div>
            <div class="flex flex-col gap-2" role="tabpanel">{body}</div>
        </div>
    }
}

/// Copies `text`, saying so in the status line.
fn copy(workspace: Workspace, what: &'static str, text: String) {
    leptos::task::spawn_local(async move {
        let done = crate::browser::copy_text(&text).await;
        workspace.status.set(if done {
            format!("{what} copied.")
        } else {
            "This browser does not let the page copy; select the text instead.".to_owned()
        });
    });
}

/// A value with a copy button.
fn copy_line(
    workspace: Workspace,
    label: &'static str,
    value: String,
    id: &'static str,
    primary: bool,
) -> impl IntoView {
    let copied = value.clone();
    view! {
        <div class="flex items-start gap-2">
            <code class=CODE id=id>{value}</code>
            <button
                type="button"
                class=if primary { PRIMARY } else { BUTTON }
                title=format!("Copy the {}", label.to_lowercase())
                data-copy=id
                on:click=move |_| copy(workspace, label, copied.clone())
            >
                <Glyph icon=icondata::LuCopy />
                "Copy"
            </button>
        </div>
    }
}

/// The question a menu's New URL, New secret, or Delete asks first.
fn confirmation(
    workspace: Workspace,
    webhook: &Webhook,
    confirm: RwSignal<Option<(WebhookId, Confirm)>>,
) -> impl IntoView + use<> {
    let id = webhook.id.clone();
    let title = webhook_title(webhook);
    move || {
        let what = confirm
            .get()
            .filter(|(asked, _)| *asked == id)
            .map(|(_, what)| what)?;
        let (id, title) = (id.clone(), title.clone());
        let question = match what {
            Confirm::Delete => format!(
                "Delete {title}? Its URL stops working at once; the sessions it started stay."
            ),
            Confirm::RotateUrl => format!(
                "Give {title} a new URL? The old one stops working at once, so update every sender."
            ),
            Confirm::RotateSecret => format!(
                "Give {title} a new secret? Requests signed with the old one are refused from now on."
            ),
        };
        let verb = match what {
            Confirm::Delete => "Delete",
            Confirm::RotateUrl => "New URL",
            Confirm::RotateSecret => "New secret",
        };
        Some(view! {
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
        })
    }
}

/// The card a new webhook's page opens on (H5): its URL and secret to
/// copy, where they go for its preset, and a test event.
fn ready_card(workspace: Workspace, webhook: &Webhook, url: String) -> impl IntoView + use<> {
    let title = webhook_title(webhook);
    let secret = workspace.webhook_revealed.with_untracked(|shown| {
        shown
            .as_ref()
            .filter(|shown| shown.webhook == webhook.id)
            .and_then(|shown| shown.secret.clone())
    });
    let steps = preset_of(webhook.verify.as_ref()).steps();
    let tested = webhook.id.clone();
    view! {
        <div class="flex flex-col gap-2 rounded-xl bg-raised p-4 shadow-[0_0_0_1px_var(--color-accent),0_0_0_4px_var(--color-accent-soft)]" role="status" id="webhook-ready">
            <div class="flex items-center gap-2">
                <span class="inline-flex h-7 w-7 items-center justify-center rounded-full bg-ok-soft text-ok">
                    <Glyph icon=icondata::LuCheck />
                </span>
                <b class="text-base">{format!("{title} is ready")}</b>
            </div>
            <p class="text-[0.8125rem] text-muted">
                {if secret.is_some() {
                    "Copy both to the sender now. The secret shows here until you leave this page; Setup's Show brings it back."
                } else {
                    "Copy the URL to the sender."
                }}
            </p>
            <span class=GROUP>"URL"</span>
            {copy_line(workspace, "URL", url, "webhook-ready-url", true)}
            {secret.map(|secret| view! {
                <span class=GROUP>"Secret"</span>
                {copy_line(workspace, "Secret", secret, "webhook-ready-secret", true)}
            })}
            <p class="rounded-lg bg-sunken px-3 py-2 text-[0.8125rem] leading-relaxed">{steps}</p>
            <div class="flex gap-2 pt-1">
                <button type="button" class=BUTTON on:click=move |_| workspace.test_webhook(tested.clone())>
                    <Glyph icon=icondata::LuSend />
                    "Send a test event"
                </button>
                <button type="button" class=BUTTON id="webhook-ready-done" on:click=move |_| workspace.webhook_revealed.set(None)>
                    "Done"
                </button>
            </div>
        </div>
    }
}

/// Setup (H2): the URL, the secret (masked until shown), the sender's
/// steps for the matching preset, and what happens to an event.
fn setup_tab(workspace: Workspace, webhook: Webhook, url: String) -> impl IntoView {
    let id = webhook.id.clone();
    let title = webhook_title(&webhook);
    let shown_secret = {
        let id = id.clone();
        move || {
            workspace.webhook_revealed.with(|shown| {
                shown
                    .as_ref()
                    .filter(|shown| shown.webhook == id)
                    .and_then(|shown| shown.secret.clone())
            })
        }
    };
    let preset = preset_of(webhook.verify.as_ref());
    let secret = webhook.verify.as_ref().map(|verify| {
        let what = match verify.kind {
            cookie_agent_protocol::WebhookVerifyKind::Bearer => "the sender puts it in each request's Authorization header",
            _ => "the sender signs each request with it",
        };
        let (shown, hidden) = (shown_secret.clone(), shown_secret.clone());
        let (revealed, copied) = ((id.clone(), title.clone()), id.clone());
        view! {
            <span class=GROUP>"Secret " <span class="font-normal normal-case tracking-normal">{format!("· {what}")}</span></span>
            <div class="flex flex-wrap items-start gap-2">
                <code class=CODE id="webhook-secret">
                    {move || shown().unwrap_or_else(|| "•".repeat(32))}
                </code>
                <div class="flex gap-2">
                    {move || if hidden().is_some() {
                        view! {
                            <button type="button" class=BUTTON data-webhook-action="hide" on:click=move |_| workspace.webhook_revealed.set(None)>
                                <Glyph icon=icondata::LuEyeOff />
                                "Hide"
                            </button>
                        }
                        .into_any()
                    } else {
                        let revealed = revealed.clone();
                        view! {
                            <button
                                type="button"
                                class=BUTTON
                                data-webhook-action="reveal"
                                on:click=move |_| workspace.reveal_webhook_secret(revealed.0.clone(), revealed.1.clone())
                            >
                                <Glyph icon=icondata::LuEye />
                                "Show"
                            </button>
                        }
                        .into_any()
                    }}
                    <button type="button" class=BUTTON data-copy="webhook-secret" on:click=move |_| workspace.copy_webhook_secret(copied.clone())>
                        <Glyph icon=icondata::LuCopy />
                        "Copy"
                    </button>
                </div>
            </div>
        }
    });
    let happens = what_happens(&webhook, name_of(workspace))
        .into_iter()
        .map(|line| {
            view! {
                <li class="flex flex-col gap-0.5 px-3 py-2">
                    <span class="text-sm font-medium">{line.heading}</span>
                    {line.detail.map(|detail| view! {
                        <span class="text-[0.8125rem] whitespace-pre-wrap break-words text-muted">{detail}</span>
                    })}
                </li>
            }
        })
        .collect_view();
    view! {
        <span class=GROUP>"URL"</span>
        {copy_line(workspace, "URL", url, "webhook-url", false)}
        {secret}
        <span class=GROUP>{format!("At the sender · {}", preset.label())}</span>
        <p class="rounded-lg bg-sunken px-3 py-2 text-[0.8125rem] leading-relaxed" data-webhook-steps>{preset.steps()}</p>
        <span class=GROUP>"What happens"</span>
        <ul class="flex flex-col divide-y divide-line rounded-xl bg-raised shadow-[0_0_0_1px_var(--color-line)]" data-webhook-happens>
            {happens}
        </ul>
    }
}

/// Events (H3): the counters, then each event newest first.
fn events_tab(workspace: Workspace, webhook: &Webhook) -> impl IntoView + use<> {
    let now = Timestamp::now();
    let zone = zone_at(now);
    let counters = counters(webhook)
        .into_iter()
        .map(|counter| counter_tile(counter, now))
        .collect_view();
    let events = if webhook.history.is_empty() {
        view! { <p class=NOTE>"No events yet. Send a test event to try it."</p> }.into_any()
    } else {
        events_newest_first(webhook)
            .map(|event| event_row(workspace, webhook, event, &zone, now))
            .collect_view()
            .into_any()
    };
    view! {
        <div class="grid grid-cols-2 gap-2 sm:grid-cols-4" data-webhook-counters>{counters}</div>
        <p class=NOTE>"Refused and filtered requests are counted, not kept."</p>
        <div class="flex flex-col divide-y divide-line rounded-xl bg-raised shadow-[0_0_0_1px_var(--color-line)]" data-webhook-history>
            {events}
        </div>
    }
}

/// One counter: what, how many, when last, and what it means.
fn counter_tile(counter: Counter, now: Timestamp) -> impl IntoView {
    let Counter {
        what,
        status,
        count,
        last,
        meaning,
        warn,
    } = counter;
    let reading = match last {
        Some(at) if count > 0 => format!("{} · {meaning}", relative_time(now, at)),
        _ => meaning.clone(),
    };
    view! {
        <div
            class=if warn { "flex flex-col gap-0.5 rounded-xl bg-warn-soft px-3 py-2" } else { "flex flex-col gap-0.5 rounded-xl bg-sunken px-3 py-2" }
            data-webhook-counter=what
            title=meaning
        >
            <span class=if warn { "text-[0.6875rem] font-semibold text-warn" } else { "text-[0.6875rem] font-semibold text-muted" }>
                {format!("{what} · {status}")}
            </span>
            <b class="text-lg leading-tight tabular-nums">{count}</b>
            <span class="text-[0.6875rem] text-muted">{reading}</span>
        </div>
    }
}

/// An event's row (H3): a dot, the outcome in words, a test tag, the time,
/// and its session; a press opens its request.
fn event_row(
    workspace: Workspace,
    webhook: &Webhook,
    event: &WebhookEvent,
    zone: &jiff::tz::TimeZone,
    now: Timestamp,
) -> impl IntoView + use<> {
    let kind = outcome_kind(event);
    let tone = match kind {
        OutcomeKind::Failed => Tone::Bad,
        OutcomeKind::Pending | OutcomeKind::Waiting => Tone::Warn,
        OutcomeKind::Unknown | OutcomeKind::Ended => Tone::Quiet,
        OutcomeKind::Delivered | OutcomeKind::Completed => Tone::Good,
    };
    let outcome = {
        let outcome = event_outcome(event);
        let outcome = outcome
            .strip_prefix("test · ")
            .unwrap_or(&outcome)
            .to_owned();
        let mut chars = outcome.chars();
        chars
            .next()
            .map(|first| first.to_uppercase().chain(chars).collect::<String>())
            .unwrap_or_default()
    };
    let mut when = vec![
        event_time(event, zone),
        relative_time(now, event.received_at),
    ];
    if let Some(key) = &event.thread_key {
        when.push(format!("thread {key}"));
    }
    let id = event.id.clone();
    let hook = webhook.id.clone();
    let path = webhook.path.clone();
    let open = {
        let (hook, id) = (hook.clone(), id.clone());
        Memo::new(move |_| {
            workspace.webhook_event.with(|open| {
                open.as_ref()
                    .is_some_and(|open| open.webhook == hook && open.event == id)
            })
        })
    };
    let toggle = {
        let (hook, id) = (hook.clone(), id.clone());
        move |_| workspace.toggle_webhook_event(hook.clone(), id.clone())
    };
    let session = event.session_id.map(|session| {
        let name = name_of(workspace)(session).unwrap_or_else(|| "its session".to_owned());
        view! {
            <button
                type="button"
                class="inline-flex min-h-11 max-w-[40%] shrink-0 items-center gap-1.5 rounded-lg px-2 text-[0.8125rem] text-accent hover:bg-hover lg:min-h-9"
                data-webhook-event-session=session.to_string()
                title="Open the session"
                on:click=move |_| {
                    workspace.open_view(View::Chat, None);
                    workspace.open_session(session);
                }
            >
                <Glyph icon=icondata::LuExternalLink class="shrink-0" />
                <span class="truncate">{name}</span>
            </button>
        }
    });
    view! {
        <div class="flex flex-col" data-webhook-event=id.clone()>
            <div class="flex items-center gap-1 pr-1">
                <button
                    type="button"
                    class="flex min-h-14 min-w-0 flex-1 items-center gap-3 rounded-lg px-3 text-left hover:bg-hover"
                    aria-expanded=move || open.get().to_string()
                    title=format!("{} · show the request", exact_time(event.received_at))
                    on:click=toggle
                >
                    <span class="inline-flex shrink-0 text-muted transition-transform" class:rotate-90=move || open.get()>
                        <Glyph icon=icondata::LuChevronRight />
                    </span>
                    <span class=format!("h-2 w-2 shrink-0 rounded-full {}", dot(tone)) aria-hidden="true"></span>
                    <span class="flex min-w-0 flex-1 flex-col">
                        <span class="flex min-w-0 items-center gap-2 text-sm font-medium" class:text-bad=tone == Tone::Bad>
                            <span class="min-w-0 break-words" data-webhook-event-outcome>{outcome}</span>
                            {event.test.then(|| view! { <span class="inline-flex h-5 shrink-0 items-center rounded-md bg-accent-soft px-1.5 text-[0.6875rem] font-semibold text-accent" data-webhook-test>"test"</span> })}
                        </span>
                        <span class="truncate text-xs text-muted">{when.join(" · ")}</span>
                    </span>
                </button>
                {session}
            </div>
            {move || open.get().then(|| event_request(workspace, path.clone()))}
        </div>
    }
}

/// The open event's request: the method and path, the headers as a table,
/// and the body, pretty-printed when it is JSON.
fn event_request(workspace: Workspace, path: String) -> impl IntoView {
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
                    .map(|header| {
                        view! {
                            <tr>
                                <td class="w-40 py-0.5 pr-3 align-top text-muted">{header.name.clone()}</td>
                                <td class="py-0.5 font-mono break-all">{header.value.clone()}</td>
                            </tr>
                        }
                    })
                    .collect_view();
                let body = match (request.body_text, request.body_base64) {
                    (Some(text), _) => serde_json::from_str::<serde_json::Value>(&text)
                        .ok()
                        .and_then(|value| serde_json::to_string_pretty(&value).ok())
                        .unwrap_or(text),
                    (None, Some(_)) => format!("({} bytes of binary content)", request.event.bytes),
                    (None, None) => "(no body)".to_owned(),
                };
                let copied = body.clone();
                view! {
                    <div class="flex flex-col gap-2 border-t border-line py-3 pr-3 pl-12" data-webhook-request>
                        <div class="flex items-center gap-2">
                            <span class="rounded-md bg-accent-soft px-1.5 py-0.5 font-mono text-[0.6875rem] font-semibold text-accent">{request.method}</span>
                            <span class="min-w-0 flex-1 truncate font-mono text-xs text-muted">{path.clone()}</span>
                            <button
                                type="button"
                                class=ICON_BUTTON
                                title="Copy the body"
                                aria-label="Copy the body"
                                on:click=move |_| copy(workspace, "Body", copied.clone())
                            >
                                <Glyph icon=icondata::LuCopy />
                            </button>
                        </div>
                        <table class="w-full text-xs" data-webhook-headers><tbody>{headers}</tbody></table>
                        <pre class="max-h-96 overflow-auto rounded-lg bg-sunken px-3 py-2 font-mono text-xs leading-relaxed whitespace-pre-wrap break-words" data-webhook-body>{body}</pre>
                    </div>
                }
                .into_any()
            }
        }
    }
}

/// Rules & limits: the webhook's details as clients list them, its
/// prompts, and when it was made.
fn rules_tab(workspace: Workspace, webhook: &Webhook, url: &str) -> impl IntoView + use<> {
    let zone = zone_at(Timestamp::now());
    let lines = workspace
        .projects
        .with_untracked(|projects| detail_lines(webhook, url, &zone, projects, name_of(workspace)))
        .into_iter()
        // The URL, the counts, and the switch have their own places.
        .filter(|detail| !matches!(detail.label, "url" | "requests" | "state" | "thread prompt"))
        .map(|detail| {
            let value = match detail.session {
                Some(session) => session_link(workspace, session, detail.value).into_any(),
                None => detail.value.into_any(),
            };
            view! {
                <dt class="text-xs font-semibold text-muted">{detail.label}</dt>
                <dd class="min-w-0 text-sm break-words" data-webhook-detail=detail.label>{value}</dd>
            }
        })
        .collect_view();
    let thread_prompt = match &webhook.target {
        WebhookTarget::New(template) => template.thread_prompt.clone(),
        _ => None,
    };
    let prompt = webhook.prompt.clone();
    let edited = webhook.clone();
    view! {
        <dl class="grid grid-cols-[auto_1fr] items-baseline gap-x-4 gap-y-1.5 pt-2">{lines}</dl>
        {thread_prompt.map(|text| view! {
            <span class=GROUP>"When a session starts"</span>
            <p class="rounded-lg bg-sunken px-3 py-2 text-sm whitespace-pre-wrap break-words" data-webhook-thread-prompt>{text}</p>
        })}
        {prompt.map(|text| view! {
            <span class=GROUP>"With every event"</span>
            <p class="rounded-lg bg-sunken px-3 py-2 text-sm whitespace-pre-wrap break-words" data-webhook-prompt>{text}</p>
        })}
        <div class="pt-2">
            <button type="button" class=BUTTON data-webhook-action="edit" on:click=move |_| workspace.edit_webhook(edited.clone())>
                <Glyph icon=icondata::LuPencil />
                "Edit"
            </button>
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
