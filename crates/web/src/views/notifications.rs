//! Settings › Notifications (push notifications spec section 9.1):
//! turning push notifications on in this browser, a test, and every device
//! the daemon pushes to.

use cookie_agent_protocol::PushSubscription;
use leptos::prelude::*;

use super::{BUTTON, ERROR, NOTE, PRIMARY};
use crate::{
    app::Connection, format::relative_time, icons::Glyph, push::PushOffer, workspace::Workspace,
};

/// The section: what this browser offers, then the devices. Opening it
/// reads `notifications.get` ([`Workspace::open_section`]).
pub(crate) fn notifications_section(workspace: Workspace, connection: Connection) -> impl IntoView {
    let offer = move || {
        workspace
            .push_key()
            .map(|key| workspace.push_context.get().offer(&key))
    };
    let label_draft = RwSignal::new(None::<String>);
    let body = move || {
        if !workspace.has_push.get() {
            return view! {
                <p class=NOTE>"This daemon is older than push notifications; update it to use them."</p>
            }
            .into_any();
        }
        let failed = workspace.notifications.with(|notifications| {
            notifications
                .as_ref()
                .and_then(|result| result.as_ref().err().cloned())
        });
        if let Some(error) = failed {
            return view! { <p class=ERROR>{error}</p> }.into_any();
        }
        let Some(offer) = offer() else {
            return view! { <p class=NOTE>"Loading…"</p> }.into_any();
        };
        let busy = move || workspace.push_busy.get();
        let url = move || connection.url();
        match offer {
            PushOffer::NeedsHttps => view! {
                <p class="flex items-start gap-2 text-sm">
                    <Glyph icon=icondata::FaLockSolid class="mt-0.5 text-caramel dark:text-maple" />
                    <span>"Notifications need HTTPS, as installing the app does."</span>
                </p>
            }
            .into_any(),
            PushOffer::InstallFirst => view! {
                <p class=NOTE>
                    "On iPhone and iPad, notifications reach only the installed app: add the page to the Home Screen (above), open it from there, and turn them on in the app."
                </p>
            }
            .into_any(),
            PushOffer::Unsupported => view! {
                <p class=NOTE>"This browser cannot receive push notifications."</p>
            }
            .into_any(),
            PushOffer::Blocked => view! {
                <p class=NOTE>
                    "Notifications are blocked for this page. Allow them in the browser's site settings, then come back here."
                </p>
            }
            .into_any(),
            PushOffer::Off => view! {
                <p class=NOTE>
                    "Get a notification when a session finishes its turn with nothing left running, or when an approval or an agent's question waits for you."
                </p>
                <div>
                    <button
                        type="button"
                        id="notifications-on"
                        class=PRIMARY
                        disabled=busy
                        on:click=move |_| workspace.turn_on_notifications(url())
                    >
                        <Glyph icon=icondata::FaBellSolid />
                        "Turn on notifications"
                    </button>
                </div>
            }
            .into_any(),
            PushOffer::OtherDaemon => view! {
                <p class=NOTE>
                    "This browser gets notifications from another daemon, or from this one's older key. A browser takes them from one daemon at a time."
                </p>
                <div>
                    <button
                        type="button"
                        id="notifications-switch"
                        class=PRIMARY
                        disabled=busy
                        on:click=move |_| workspace.turn_on_notifications(url())
                    >
                        <Glyph icon=icondata::FaBellSolid />
                        "Switch to this daemon"
                    </button>
                </div>
            }
            .into_any(),
            PushOffer::On => {
                let own = workspace.own_subscription(&url());
                let current_label = move || {
                    let own = workspace.own_subscription(&url());
                    workspace.notifications.with(|notifications| {
                        notifications
                            .as_ref()
                            .and_then(|result| result.as_ref().ok())
                            .and_then(|result| {
                                result
                                    .subscriptions
                                    .iter()
                                    .find(|shown| Some(&shown.id) == own.as_ref())
                                    .map(|shown| shown.label.clone())
                            })
                            .unwrap_or_default()
                    })
                };
                view! {
                    <p class="flex items-center gap-2 text-sm">
                        <Glyph icon=icondata::FaCircleCheckSolid class="text-green-700 dark:text-green-400" />
                        "Notifications are on in this browser."
                    </p>
                    <label class="flex flex-col gap-1 text-sm">
                        <span class=NOTE>"This device's name in the list"</span>
                        <input
                            id="notifications-label"
                            class="min-h-11 rounded-md border border-crust bg-cream px-3 text-sm dark:border-smoke dark:bg-night"
                            maxlength="80"
                            prop:value=move || label_draft.get().unwrap_or_else(current_label)
                            on:input=move |event| label_draft.set(Some(event_target_value(&event)))
                            on:change=move |_| {
                                if let Some(label) = label_draft.get_untracked() {
                                    workspace.rename_subscription(url(), label);
                                    label_draft.set(None);
                                }
                            }
                        />
                    </label>
                    <div class="flex flex-wrap gap-2">
                        {own.map(|id| view! {
                            <button
                                type="button"
                                id="notifications-test"
                                class=BUTTON
                                on:click=move |_| workspace.test_subscription(id.clone())
                            >
                                <Glyph icon=icondata::FaPaperPlaneSolid />
                                "Send test"
                            </button>
                        })}
                        <button
                            type="button"
                            id="notifications-off"
                            class=BUTTON
                            disabled=busy
                            on:click=move |_| workspace.turn_off_notifications(url())
                        >
                            <Glyph icon=icondata::FaBellSlashSolid />
                            "Turn off"
                        </button>
                    </div>
                }
                .into_any()
            }
        }
    };
    let sending_off = move || {
        workspace.notifications.with(|notifications| {
            notifications
                .as_ref()
                .and_then(|result| result.as_ref().ok())
                .is_some_and(|result| !result.enabled)
        })
    };
    let devices = move || {
        workspace.notifications.with(|notifications| {
            notifications
                .as_ref()
                .and_then(|result| result.as_ref().ok())
                .map(|result| result.subscriptions.clone())
                .unwrap_or_default()
        })
    };
    let loaded = move || {
        workspace
            .notifications
            .with(|notifications| notifications.as_ref().is_some_and(Result::is_ok))
    };
    view! {
        <h2 class="text-sm font-bold">"This browser"</h2>
        <div id="notifications" class="flex flex-col gap-2">
            {body}
            <Show when=sending_off>
                <p class=NOTE>"The daemon sends none now: " <code class="font-mono text-xs">"[notifications] enabled = false"</code>"."</p>
            </Show>
            {move || workspace.push_test.get().map(|said| view! { <p class=NOTE data-notifications-test>{said}</p> })}
            {move || workspace.push_error.get().map(|error| view! { <p class=ERROR>{error}</p> })}
        </div>
        <Show when=move || workspace.has_push.get() && loaded()>
            <h2 class="mt-4 text-sm font-bold">"Devices"</h2>
            <p class=NOTE>"Every browser this daemon sends notifications to."</p>
            <Show
                when=move || !devices().is_empty()
                fallback=|| view! { <p class=NOTE id="notifications-no-devices">"No devices yet."</p> }
            >
                <ul id="notifications-devices" class="flex flex-col gap-1">
                    <For each=devices key=|device| device.id.clone() let:device>
                        {device_row(workspace, connection, device)}
                    </For>
                </ul>
            </Show>
        </Show>
    }
}

/// A device the daemon pushes to: its label, push service, when it last
/// got one, why the last failed, and **Remove**.
fn device_row(
    workspace: Workspace,
    connection: Connection,
    device: PushSubscription,
) -> impl IntoView {
    let now = jiff::Timestamp::now();
    let mine = workspace.own_subscription(&connection.url()).as_deref() == Some(device.id.as_str());
    let sent = device.last_sent_at.map_or_else(
        || "nothing sent yet".to_owned(),
        |at| format!("last sent {}", relative_time(now, at)),
    );
    let id = device.id.clone();
    view! {
        <li class="flex items-center gap-2 rounded-md border border-toasted px-3 py-2 dark:border-ember" data-push-subscription=device.id.clone()>
            <div class="flex min-w-0 flex-1 flex-col">
                <span class="truncate text-sm font-bold">
                    {device.label.clone()}
                    {mine.then(|| view! { <span class="ml-2 text-xs font-normal">"(this browser)"</span> })}
                </span>
                <span class=NOTE>{format!("{} · {sent}", device.push_service)}</span>
                {device.last_error.clone().map(|error| view! { <span class="text-xs text-red-800 dark:text-red-300">{error}</span> })}
            </div>
            <button
                type="button"
                class=BUTTON
                aria-label=format!("Remove {}", device.label)
                on:click=move |_| workspace.remove_subscription(id.clone())
            >
                "Remove"
            </button>
        </li>
    }
}
