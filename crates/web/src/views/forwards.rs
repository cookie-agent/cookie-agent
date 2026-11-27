//! The Forwards view (client protocol 1.109.0,
//! `docs/site/specs/forwards.md` section 8.1): every forward as a row with
//! its route, status, traffic, and expiry, and *Open* for a web app; an
//! open forward as a page with its Overview (address, expiry, share links,
//! Delete) and its recent Connections; the New forward form; and the
//! session panel's Forwards tab.

use cookie_agent_client_state::{
    format::relative_time,
    forward::{
        ForwardLifetime, expiry_text, forward_title, kind_label, status_text, traffic_text,
        tree_forwards,
    },
    memory::exact_time,
    shares::ShareLifetime,
};
use cookie_agent_protocol::{
    Forward, ForwardCreateParams, ForwardKind, ForwardListen, ForwardStatus, ForwardTarget,
    human_bytes,
};
use jiff::Timestamp;
use leptos::prelude::*;

use super::{
    BUTTON, ERROR, ICON_BUTTON, NOTE, PRIMARY, WARNING,
    cron::SELECT,
    empty_state, loading,
    manage::{CONFIRM, EDITOR_ACTIONS, EDITOR_PAGE, FIELD},
    row_menu::{MenuItem, row_menu},
};
use crate::{
    icons::Glyph,
    layout::View,
    workspace::{ForwardTab, Workspace},
};

/// A small tag after a name.
const TAG: &str = "inline-flex h-5 shrink-0 items-center rounded-md bg-sunken px-1.5 text-[0.6875rem] font-semibold text-muted";

/// A value to copy: an address, a link.
const CODE: &str =
    "min-w-0 flex-1 rounded-lg bg-sunken px-3 py-2 font-mono text-xs break-all whitespace-pre-wrap";

/// A row's ⋯, kept clear of the row's open button under it.
const MORE: &str = "relative z-[1] inline-flex min-h-11 min-w-11 shrink-0 items-center justify-center rounded-lg text-muted transition-colors hover:bg-hover hover:text-fg active:bg-press lg:min-h-9 lg:min-w-9";

/// A group's heading on a forward's page.
const GROUP: &str = "pt-3 text-xs font-semibold tracking-wide text-muted uppercase";

/// The form's rows: the label on the left in a wide window, above on a
/// phone.
const GRID: &str = "grid items-start gap-x-4 gap-y-2 sm:grid-cols-[9.5rem_1fr]";

/// A row's label in [`GRID`].
const LABEL: &str = "pt-2.5 text-[0.8125rem] font-medium text-fg-2 sm:pt-2";

/// One option of a segmented choice, and the choice around them.
const SEGMENT: &str = "inline-flex min-h-11 items-center rounded-md px-3 text-[0.8125rem] text-muted transition-colors hover:text-fg aria-checked:bg-raised aria-checked:font-semibold aria-checked:text-fg aria-checked:shadow-[0_0_0_1px_var(--color-line-2),0_1px_2px_rgb(0_0_0/0.06)] lg:min-h-8";
const SEGMENTS: &str = "inline-flex w-fit shrink-0 gap-0.5 rounded-lg bg-sunken p-0.5";

/// A picker as wide as its choices, beside its button.
const PICK: &str = "min-h-11 w-auto rounded-lg bg-surface px-2 text-sm hover:shadow-[inset_0_0_0_1px_var(--color-faint)] focus:shadow-[inset_0_0_0_1px_var(--color-accent),0_0_0_3px_var(--color-accent-soft)] focus:outline-none shadow-[inset_0_0_0_1px_var(--color-line-2)] lg:min-h-9";

/// The icon a forward is drawn with.
const fn kind_icon(kind: ForwardKind) -> icondata::Icon {
    match kind {
        ForwardKind::Http => icondata::LuGlobe,
        ForwardKind::Tcp => icondata::LuCable,
        ForwardKind::Udp => icondata::LuRadioTower,
    }
}

/// The dot of a status.
const fn dot(status: ForwardStatus) -> &'static str {
    match status {
        ForwardStatus::Active => "bg-ok",
        ForwardStatus::Waiting => "bg-warn",
        ForwardStatus::Failed => "bg-bad",
        ForwardStatus::Expired => "bg-faint",
    }
}

pub fn forwards_page(workspace: Workspace) -> impl IntoView {
    let menu = RwSignal::new(None::<String>);
    move || {
        if workspace.forward_form.get() {
            return forward_form(workspace).into_any();
        }
        let listing = match workspace.forwards.get() {
            None => return loading("Reading forwards…").into_any(),
            Some(Err(error)) => return view! { <p class=ERROR>{error}</p> }.into_any(),
            Some(Ok(listing)) => listing,
        };
        if let Some(forward) = workspace.forward_selected.get().and_then(|handle| {
            listing
                .forwards
                .iter()
                .find(|forward| forward.handle == handle)
                .cloned()
        }) {
            return forward_page(workspace, forward, menu).into_any();
        }
        let rows = if listing.forwards.is_empty() {
            empty_state(
                icondata::LuArrowRightLeft,
                "No forwards. Agents make them with their forward tools, or add one here.",
            )
            .into_any()
        } else {
            view! {
                <div class="flex flex-col gap-2" data-forward-list>
                    {listing
                        .forwards
                        .into_iter()
                        .map(|forward| forward_row(workspace, forward, menu))
                        .collect_view()}
                </div>
            }
            .into_any()
        };
        view! {
            {super::view_header(
                "Forwards",
                "A machine's web app at this page's address, and TCP or UDP ports of one machine on another.",
                view! {
                    <button type="button" id="forward-new" class=PRIMARY on:click=move |_| workspace.new_forward()>
                        <Glyph icon=icondata::LuPlus />
                        "New forward"
                    </button>
                },
            )}
            {error_line(workspace)}
            {rows}
        }
        .into_any()
    }
}

fn error_line(workspace: Workspace) -> impl IntoView {
    move || {
        workspace
            .forward_error
            .get()
            .map(|error| view! { <p class=ERROR id="forward-error">{error}</p> })
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

/// Where a forward is reached from here: the web app's URL, or the
/// `bind:port` it listens on.
fn reach(workspace: Workspace, forward: &Forward) -> Option<String> {
    match forward.kind {
        ForwardKind::Http => workspace.forward_url(forward),
        ForwardKind::Tcp | ForwardKind::Udp => {
            forward.address.clone().map(|address| match &forward.from {
                Some(from) => format!("{address} on {}", from.machine),
                None => address,
            })
        }
    }
}

/// *Open* of a web forward: its app in a new tab.
fn open_link(
    workspace: Workspace,
    forward: &Forward,
    class: &'static str,
) -> Option<impl IntoView + use<>> {
    if forward.kind != ForwardKind::Http || forward.status == ForwardStatus::Expired {
        return None;
    }
    let url = workspace.forward_url(forward)?;
    Some(view! {
        <a
            class=class
            href=url
            target="_blank"
            rel="noopener"
            data-forward-action="open"
            on:click=|event: leptos::ev::MouseEvent| event.stop_propagation()
        >
            <Glyph icon=icondata::LuExternalLink />
            "Open"
        </a>
    })
}

/// A forward's actions, for its ⋯: Open its page, Copy the address, and
/// Delete, which asks on its page first.
fn menu_items(
    workspace: Workspace,
    forward: &Forward,
) -> impl Fn() -> Vec<MenuItem> + Send + Sync + Clone + 'static + use<> {
    let handle = forward.handle.clone();
    let address = reach(workspace, forward);
    move || {
        let mut items = Vec::new();
        if let Some(address) = address.clone() {
            items.push(MenuItem {
                id: "copy",
                label: "Copy address",
                icon: icondata::LuCopy,
                act: Callback::new(move |()| copy(workspace, "Address", address.clone())),
                danger: false,
            });
        }
        let deleted = handle.clone();
        items.push(MenuItem {
            id: "delete",
            label: "Delete…",
            icon: icondata::LuTrash2,
            act: Callback::new(move |()| {
                if workspace.forward_selected.get_untracked().as_deref() != Some(&deleted)
                    || workspace.view.get_untracked() != View::Forwards
                {
                    workspace.open_forward(deleted.clone());
                }
                workspace.forward_confirm.set(Some(deleted.clone()));
            }),
            danger: true,
        });
        items
    }
}

/// A row's status line: the dot, the status, the traffic, the expiry.
fn status_line(forward: &Forward, now: Timestamp) -> impl IntoView + use<> {
    let tone = if forward.status == ForwardStatus::Failed {
        "text-bad"
    } else {
        "text-muted"
    };
    view! {
        <span class="flex min-w-0 flex-wrap items-center gap-x-3 gap-y-0.5 text-xs" data-forward-status=format!("{:?}", forward.status).to_lowercase()>
            <span class=format!("inline-flex min-w-0 items-center gap-1.5 {tone}")>
                <span class=format!("h-2 w-2 shrink-0 rounded-full {}", dot(forward.status)) aria-hidden="true"></span>
                {status_text(forward)}
            </span>
            <span class="text-muted" data-forward-traffic>{traffic_text(forward)}</span>
            <span class="text-muted" data-forward-expiry title=forward.expires_at.map(exact_time)>{expiry_text(forward, now)}</span>
        </span>
    }
}

/// A forward's row: a press opens its page, for the user.
fn forward_row(
    workspace: Workspace,
    forward: Forward,
    menu: RwSignal<Option<String>>,
) -> impl IntoView {
    let now = Timestamp::now();
    let title = forward_title(&forward);
    let handle = forward.handle.clone();
    let guest = workspace.is_guest();
    let open = {
        let handle = handle.clone();
        move |_| workspace.open_forward(handle.clone())
    };
    let items = menu_items(workspace, &forward);
    let route = forward.route();
    view! {
        <div
            class="relative flex items-start gap-3 rounded-xl bg-raised py-2 pr-1 pl-3 shadow-[0_0_0_1px_var(--color-line),0_1px_2px_rgb(0_0_0/0.03)] transition-colors hover:bg-hover"
            class:opacity-70=forward.status == ForwardStatus::Expired
            data-forward=handle.clone()
            data-title=title.clone()
        >
            {(!guest).then(|| view! {
                <button
                    type="button"
                    class="absolute inset-0 rounded-xl"
                    aria-label=format!("Open {title}")
                    data-forward-open
                    on:click=open
                ></button>
            })}
            <span class="mt-1.5 inline-flex h-8 w-8 shrink-0 items-center justify-center rounded-lg bg-sunken text-fg-2">
                <Glyph icon=kind_icon(forward.kind) />
            </span>
            <span class="flex min-w-0 flex-1 flex-col gap-0.5 py-1">
                <span class="flex min-w-0 items-center gap-2">
                    <span class="min-w-0 truncate text-sm font-semibold" data-forward-title>{title.clone()}</span>
                    <span class=TAG>{kind_label(forward.kind)}</span>
                </span>
                {forward.title.is_some().then(|| view! {
                    <span class="block min-w-0 truncate font-mono text-xs text-fg-2" data-forward-route>{route}</span>
                })}
                {status_line(&forward, now)}
            </span>
            {open_link(workspace, &forward, "relative z-[1] inline-flex min-h-11 shrink-0 items-center gap-1 rounded-lg px-2 text-[0.8125rem] font-medium text-accent hover:bg-hover lg:min-h-9")}
            {(!guest).then(|| row_menu(menu, handle.clone(), handle.clone(), title.clone(), MORE, items))}
        </div>
    }
}

/// An open forward: its head, its actions, Delete's question, and its two
/// tabs.
fn forward_page(
    workspace: Workspace,
    forward: Forward,
    menu: RwSignal<Option<String>>,
) -> impl IntoView {
    let title = forward_title(&forward);
    let tab = workspace.forward_tab;
    let items = menu_items(workspace, &forward);
    let address = reach(workspace, &forward);
    let connections = forward.recent.len();
    let tab_button = move |which: ForwardTab, label: &'static str, count: Option<usize>| {
        view! {
            <button
                type="button"
                role="tab"
                class="inline-flex min-h-11 shrink-0 items-center gap-1.5 border-b-2 px-2 text-[0.8125rem] whitespace-nowrap transition-colors aria-selected:border-accent aria-selected:font-semibold aria-selected:text-fg border-transparent text-muted hover:text-fg lg:min-h-9"
                aria-selected=move || (tab.get() == which).to_string()
                data-forward-tab=label
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
        let forward = forward.clone();
        move || match tab.get() {
            ForwardTab::Overview => overview_tab(workspace, &forward).into_any(),
            ForwardTab::Connections => connections_tab(&forward).into_any(),
        }
    };
    view! {
        <div class="flex flex-col gap-3" id="forward-page" data-forward-page=forward.handle.clone()>
            <div class="flex items-center gap-3">
                <span class="inline-flex h-10 w-10 shrink-0 items-center justify-center rounded-xl bg-sunken text-fg-2">
                    <Glyph icon=kind_icon(forward.kind) class="text-lg" />
                </span>
                <div class="min-w-0 flex-1">
                    <h2 class="truncate text-xl font-semibold tracking-tight" id="forward-page-title">{title.clone()}</h2>
                    <p class="truncate font-mono text-[0.8125rem] text-muted" data-forward-subtitle>{forward.route()}</p>
                </div>
                {row_menu(menu, forward.handle.clone(), forward.handle.clone(), title.clone(), ICON_BUTTON, items)}
            </div>
            <div class="flex gap-2" data-forward-actions>
                {open_link(workspace, &forward, PRIMARY)}
                {address.map(|address| view! {
                    <button
                        type="button"
                        class=BUTTON
                        data-forward-action="copy"
                        on:click=move |_| copy(workspace, "Address", address.clone())
                    >
                        <Glyph icon=icondata::LuCopy />
                        "Copy address"
                    </button>
                })}
            </div>
            {error_line(workspace)}
            {confirmation(workspace, &forward)}
            <div class="flex gap-1 overflow-x-auto border-b border-line" role="tablist" aria-label="Forward">
                {tab_button(ForwardTab::Overview, "Overview", None)}
                {tab_button(ForwardTab::Connections, "Connections", Some(connections))}
            </div>
            <div class="flex flex-col gap-2" role="tabpanel">{body}</div>
        </div>
    }
}

/// The question Delete asks first.
fn confirmation(workspace: Workspace, forward: &Forward) -> impl IntoView + use<> {
    let handle = forward.handle.clone();
    let title = forward_title(forward);
    let shared = forward.kind == ForwardKind::Http;
    move || {
        workspace
            .forward_confirm
            .get()
            .filter(|asked| *asked == handle)?;
        let handle = handle.clone();
        let question = if shared {
            format!(
                "Delete {title}? Its connections close, and its address and share links stop working at once."
            )
        } else {
            format!("Delete {title}? It stops listening, and its connections close at once.")
        };
        Some(view! {
            <div class=CONFIRM role="alertdialog" data-forward-confirm>
                <p class="text-sm">{question}</p>
                <div class="flex gap-2">
                    <button type="button" id="forward-confirm" class=PRIMARY on:click=move |_| {
                        workspace.forward_confirm.set(None);
                        workspace.delete_forward(handle.clone());
                    }>
                        "Delete"
                    </button>
                    <button type="button" class=BUTTON on:click=move |_| workspace.forward_confirm.set(None)>
                        "Keep it"
                    </button>
                </div>
            </div>
        })
    }
}

/// One `label: value` line of the Overview.
fn fact(label: &'static str, value: impl IntoView + 'static) -> impl IntoView {
    view! {
        <div class="flex flex-col gap-0.5 sm:flex-row sm:gap-4">
            <span class="w-32 shrink-0 text-[0.8125rem] font-medium text-fg-2">{label}</span>
            <span class="min-w-0 text-[0.8125rem]">{value}</span>
        </div>
    }
}

/// Who made a forward: the user, or an agent in a session, linked.
fn made_by(workspace: Workspace, forward: &Forward, now: Timestamp) -> impl IntoView + use<> {
    let when = relative_time(now, forward.created.at);
    let exact = exact_time(forward.created.at);
    let by = match (forward.created.session, &forward.created.agent) {
        (Some(session), agent) => {
            let name = workspace
                .sessions
                .with_untracked(|sessions| crate::cron::session_name(sessions, session));
            let who = agent
                .as_ref()
                .map_or_else(|| "an agent".to_owned(), |agent| agent.to_string());
            view! {
                {format!("{who} in ")}
                <button
                    type="button"
                    class="hit-area font-medium text-accent underline-offset-2 hover:underline"
                    data-forward-session=session.to_string()
                    on:click=move |_| {
                        workspace.open_view(View::Chat, None);
                        workspace.open_session(session);
                    }
                >
                    {name.unwrap_or_else(|| session.to_string())}
                </button>
            }
            .into_any()
        }
        (None, _) => view! { "you" }.into_any(),
    };
    view! {
        {by}
        <span class="text-muted" title=exact>{format!(", {when}")}</span>
    }
}

/// The Overview tab: the route, where it is reached, its status, its
/// expiry with a change, who made it, its share links, and Delete.
fn overview_tab(workspace: Workspace, forward: &Forward) -> impl IntoView + use<> {
    let now = Timestamp::now();
    let guest = workspace.is_guest();
    let handle = forward.handle.clone();
    let lifetime = RwSignal::new(ForwardLifetime::default());
    let change = {
        let handle = handle.clone();
        move |_| workspace.set_forward_lifetime(handle.clone(), lifetime.get_untracked())
    };
    let address = reach(workspace, forward);
    let deleted = handle.clone();
    view! {
        <div class="flex flex-col gap-2" data-forward-overview>
            {fact("Route", view! { <span class="font-mono text-xs">{forward.route()}</span> })}
            {address.map(|address| fact("Address", view! { <code class=CODE data-forward-address>{address}</code> }))}
            {fact("Status", status_text(forward))}
            {fact("Traffic", traffic_text(forward))}
            {fact("Made by", made_by(workspace, forward, now))}
            {fact("Expiry", view! {
                <span class="flex flex-wrap items-center gap-2">
                    <span data-forward-expiry title=forward.expires_at.map(exact_time)>{expiry_text(forward, now)}</span>
                    {(!guest).then(|| view! {
                        <select
                            class=PICK
                            aria-label="New expiry"
                            data-forward-lifetime
                            on:change=move |event| {
                                let chosen = event_target_value(&event);
                                if let Some(chosen) = ForwardLifetime::from_code(&chosen) {
                                    lifetime.set(chosen);
                                }
                            }
                        >
                            {ForwardLifetime::ALL.map(|choice| view! {
                                <option value=choice.code() selected=choice == ForwardLifetime::default()>
                                    {match choice {
                                        ForwardLifetime::Never => "Never ends".to_owned(),
                                        other => format!("{} from now", other.label()),
                                    }}
                                </option>
                            })}
                        </select>
                        <button type="button" class=BUTTON data-forward-action="expiry" on:click=change>
                            "Change"
                        </button>
                    })}
                </span>
            })}
            {(forward.kind == ForwardKind::Http && !guest).then(|| shares_group(workspace, handle.clone()))}
            {(!guest).then(|| view! {
                <div class="pt-3">
                    <button
                        type="button"
                        class=BUTTON
                        data-forward-action="delete"
                        on:click=move |_| workspace.forward_confirm.set(Some(deleted.clone()))
                    >
                        <Glyph icon=icondata::LuTrash2 class="text-bad" />
                        "Delete…"
                    </button>
                </div>
            })}
        </div>
    }
}

/// A web forward's share links: each with Copy and Revoke, and a new one
/// with its expiry (section 10: a link is public).
fn shares_group(workspace: Workspace, handle: String) -> impl IntoView {
    let lifetime = RwSignal::new(ShareLifetime::default());
    let made = handle.clone();
    let links = move || {
        let now = Timestamp::now();
        workspace
            .forward_links(&handle)
            .into_iter()
            .map(|link| {
                let address = workspace.forward_share_address(&link);
                let copied = address.clone();
                let id = link.id.clone();
                let ends = link.expires_at.map_or_else(
                    || "never ends".to_owned(),
                    |at| format!("ends {}", cookie_agent_client_state::cron::until(at, now)),
                );
                let views = match link.views {
                    0 => "not opened yet".to_owned(),
                    1 => "opened once".to_owned(),
                    views => format!("opened {views} times"),
                };
                view! {
                    <div class="flex flex-col gap-1" data-forward-share=link.id.to_string()>
                        <div class="flex items-start gap-2">
                            <code class=CODE>{address}</code>
                            <button type="button" class=BUTTON title="Copy the link" on:click=move |_| copy(workspace, "Link", copied.clone())>
                                <Glyph icon=icondata::LuCopy />
                            </button>
                            <button type="button" class=BUTTON title="Revoke the link" data-forward-revoke on:click=move |_| workspace.revoke_share(id.clone())>
                                "Revoke"
                            </button>
                        </div>
                        <span class=NOTE>{format!("{ends} · {views}")}</span>
                    </div>
                }
            })
            .collect_view()
    };
    view! {
        <span class=GROUP>"Share links"</span>
        <div class=WARNING role="note">
            <Glyph icon=icondata::LuTriangleAlert class="mt-0.5 shrink-0 text-warn" />
            <p class="min-w-0">"Anyone with a link reaches the app, logged in or not, until the link or the forward ends."</p>
        </div>
        {links}
        <div class="flex flex-wrap items-center gap-2">
            <select
                class=PICK
                aria-label="The link's expiry"
                data-forward-share-lifetime
                on:change=move |event| {
                    if let Some(chosen) = ShareLifetime::from_code(&event_target_value(&event)) {
                        lifetime.set(chosen);
                    }
                }
            >
                {ShareLifetime::ALL.map(|choice| view! {
                    <option value=choice.code() selected=choice == ShareLifetime::default()>
                        {match choice {
                            ShareLifetime::Never => "Never ends".to_owned(),
                            other => format!("Ends in {}", other.label()),
                        }}
                    </option>
                })}
            </select>
            <button
                type="button"
                class=BUTTON
                data-forward-action="share"
                on:click=move |_| workspace.share_forward(made.clone(), lifetime.get_untracked())
            >
                <Glyph icon=icondata::LuShare2 />
                "Make a link"
            </button>
        </div>
    }
}

/// The Connections tab: the latest connections, newest first.
fn connections_tab(forward: &Forward) -> impl IntoView + use<> {
    let now = Timestamp::now();
    if forward.recent.is_empty() {
        return view! { <p class=NOTE data-forward-connections-empty>"No connections yet."</p> }
            .into_any();
    }
    forward
        .recent
        .iter()
        .map(|connection| {
            let state = match (&connection.ended_at, &connection.ended) {
                (None, _) => "open".to_owned(),
                (Some(at), Some(how)) => format!("{how} {}", relative_time(now, *at)),
                (Some(at), None) => format!("closed {}", relative_time(now, *at)),
            };
            view! {
                <div class="flex flex-col gap-0.5 rounded-lg bg-raised px-3 py-2 shadow-[0_0_0_1px_var(--color-line)]" data-forward-connection>
                    <span class="flex min-w-0 items-center gap-2 text-[0.8125rem]">
                        <span class=format!("h-2 w-2 shrink-0 rounded-full {}", if connection.ended_at.is_none() { "bg-ok" } else { "bg-faint" }) aria-hidden="true"></span>
                        <span class="min-w-0 truncate font-mono text-xs">{connection.peer.clone()}</span>
                        <span class="ml-auto shrink-0 text-xs text-muted" title=exact_time(connection.at)>{relative_time(now, connection.at)}</span>
                    </span>
                    <span class=NOTE>{format!(
                        "{state} · {} in · {} out",
                        human_bytes(connection.bytes_in),
                        human_bytes(connection.bytes_out)
                    )}</span>
                </div>
            }
        })
        .collect_view()
        .into_any()
}

/// A text field of the form.
fn text_field(
    id: &'static str,
    value: RwSignal<String>,
    placeholder: &'static str,
    numeric: bool,
) -> impl IntoView {
    view! {
        <input
            type="text"
            id=id
            class=FIELD
            placeholder=placeholder
            inputmode=if numeric { "numeric" } else { "text" }
            autocomplete="off"
            spellcheck="false"
            prop:value=move || value.get()
            on:input=move |event| value.set(event_target_value(&event))
        />
    }
}

/// A machine picker of the form.
fn machine_field(
    id: &'static str,
    value: RwSignal<String>,
    machines: Memo<Vec<String>>,
) -> impl IntoView {
    view! {
        <select
            id=id
            class=SELECT
            on:change=move |event| value.set(event_target_value(&event))
        >
            {move || machines.get().into_iter().map(|name| {
                let selected = value.get_untracked() == name;
                view! { <option value=name.clone() selected=selected>{name.clone()}</option> }
            }).collect_view()}
        </select>
    }
}

/// A port as the form reads it: `0` only where `zero` allows it.
fn port(text: &str, what: &str, zero: bool) -> Result<u16, String> {
    let text = text.trim();
    if text.is_empty() && zero {
        return Ok(0);
    }
    match text.parse::<u16>() {
        Ok(0) if !zero => Err(format!("{what} is a port from 1 to 65535")),
        Ok(port) => Ok(port),
        Err(_) => Err(format!("{what} is a port from 1 to 65535, not {text:?}")),
    }
}

/// The New forward form: the kind, where its connections go, where a TCP
/// or UDP one listens, its title, and its expiry.
fn forward_form(workspace: Workspace) -> impl IntoView {
    let machines = Memo::new(move |_| {
        workspace.machines.with(|machines| {
            machines
                .as_ref()
                .and_then(|machines| machines.as_ref().ok())
                .map(|machines| {
                    machines
                        .machines
                        .iter()
                        .map(|machine| machine.name.as_str().to_owned())
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default()
        })
    });
    let first = move || {
        machines
            .get_untracked()
            .first()
            .cloned()
            .unwrap_or_default()
    };
    let kind = RwSignal::new(ForwardKind::Http);
    let to_machine = RwSignal::new(first());
    let to_host = RwSignal::new("127.0.0.1".to_owned());
    let to_port = RwSignal::new(String::new());
    let from_machine = RwSignal::new(first());
    let from_bind = RwSignal::new("127.0.0.1".to_owned());
    let from_port = RwSignal::new(String::new());
    let title = RwSignal::new(String::new());
    let lifetime = RwSignal::new(ForwardLifetime::default());
    // The machines may come after the form opens.
    Effect::new(move |_| {
        let names = machines.get();
        for field in [to_machine, from_machine] {
            if !names.contains(&field.get_untracked())
                && let Some(name) = names.first()
            {
                field.set(name.clone());
            }
        }
    });
    if workspace.machines.with_untracked(Option::is_none) {
        workspace.poll_runtimes();
    }
    let send = move |_| {
        let kind = kind.get_untracked();
        let built = (|| {
            let to = ForwardTarget {
                machine: to_machine.get_untracked(),
                host: to_host.get_untracked().trim().to_owned(),
                port: port(&to_port.get_untracked(), "The target's port", false)?,
            };
            if to.machine.is_empty() {
                return Err("Pick the machine the app or port is on.".to_owned());
            }
            if to.host.is_empty() {
                return Err("Name the host the machine reaches, such as 127.0.0.1.".to_owned());
            }
            let from = match kind {
                ForwardKind::Http => None,
                ForwardKind::Tcp | ForwardKind::Udp => Some(ForwardListen {
                    machine: from_machine.get_untracked(),
                    bind: from_bind.get_untracked().trim().to_owned(),
                    port: port(&from_port.get_untracked(), "The listening port", true)?,
                }),
            };
            let title = title.get_untracked().trim().to_owned();
            Ok(ForwardCreateParams {
                kind,
                to,
                from,
                title: (!title.is_empty()).then_some(title),
                expires_in_hours: Some(lifetime.get_untracked().hours()),
            })
        })();
        match built {
            Ok(params) => workspace.create_forward(params),
            Err(error) => workspace.forward_error.set(Some(error)),
        }
    };
    let segment = move |which: ForwardKind| {
        view! {
            <button
                type="button"
                role="radio"
                class=SEGMENT
                aria-checked=move || (kind.get() == which).to_string()
                data-forward-kind=which.as_str()
                on:click=move |_| kind.set(which)
            >
                {kind_label(which)}
            </button>
        }
    };
    let listens = move || kind.get() != ForwardKind::Http;
    view! {
        <div class=EDITOR_PAGE id="forward-form">
            <div class="flex flex-col gap-1 pb-2">
                <h2 class="text-xl font-semibold tracking-tight">"New forward"</h2>
                <p class="text-[0.8125rem] text-muted">
                    {move || if listens() {
                        "A port of one machine that carries its connections to an address another machine reaches."
                    } else {
                        "A web app a machine reaches, served at this page's address below /p/. It shares this page's origin, so open only apps you trust."
                    }}
                </p>
            </div>
            <div class=GRID>
                <span class=LABEL>"Kind"</span>
                <div class=SEGMENTS role="radiogroup" aria-label="Kind">
                    {segment(ForwardKind::Http)}
                    {segment(ForwardKind::Tcp)}
                    {segment(ForwardKind::Udp)}
                </div>
                <label class=LABEL for="forward-to-machine">{move || if listens() { "Connect from" } else { "App on" }}</label>
                {machine_field("forward-to-machine", to_machine, machines)}
                <label class=LABEL for="forward-to-host">"Host and port"</label>
                <div class="flex gap-2">
                    {text_field("forward-to-host", to_host, "127.0.0.1", false)}
                    <span class="w-28 shrink-0">{text_field("forward-to-port", to_port, "5173", true)}</span>
                </div>
                <Show when=listens>
                    <label class=LABEL for="forward-from-machine">"Listen on"</label>
                    {machine_field("forward-from-machine", from_machine, machines)}
                    <label class=LABEL for="forward-from-bind">"Bind and port"</label>
                    <div class="flex flex-col gap-1">
                        <div class="flex gap-2">
                            {text_field("forward-from-bind", from_bind, "127.0.0.1", false)}
                            <span class="w-28 shrink-0">{text_field("forward-from-port", from_port, "any", true)}</span>
                        </div>
                        <span class=NOTE>"An empty port picks a free one. An address other than 127.0.0.1 opens the port to that machine's network."</span>
                    </div>
                </Show>
                <label class=LABEL for="forward-title">"Title"</label>
                {text_field("forward-title", title, "Optional", false)}
                <label class=LABEL for="forward-lifetime">"Ends"</label>
                <select
                    id="forward-lifetime"
                    class=PICK
                    on:change=move |event| {
                        if let Some(chosen) = ForwardLifetime::from_code(&event_target_value(&event)) {
                            lifetime.set(chosen);
                        }
                    }
                >
                    {ForwardLifetime::ALL.map(|choice| view! {
                        <option value=choice.code() selected=choice == ForwardLifetime::default()>
                            {match choice {
                                ForwardLifetime::Never => "Never".to_owned(),
                                other => format!("In {}", other.label()),
                            }}
                        </option>
                    })}
                </select>
            </div>
            {error_line(workspace)}
            <div class="flex-1"></div>
            <div class=EDITOR_ACTIONS>
                <button type="button" id="forward-save" class=PRIMARY on:click=send>
                    "Create forward"
                </button>
                <button type="button" id="forward-cancel" class=BUTTON on:click=move |_| {
                    workspace.forward_form.set(false);
                    workspace.forward_error.set(None);
                }>
                    "Cancel"
                </button>
            </div>
        </div>
    }
}

/// The session panel's Forwards tab: the forwards the open session's tree
/// made, as the view's rows.
pub(super) fn forwards_sheet(workspace: Workspace) -> impl IntoView {
    let menu = RwSignal::new(None::<String>);
    let guest = workspace.is_guest();
    let body = move || {
        if workspace.new_session.get() {
            return view! { <p class=NOTE>"Open a session to see its forwards."</p> }.into_any();
        }
        let Some(session) = workspace.selected.get() else {
            return view! { <p class=NOTE>"Open a session to see its forwards."</p> }.into_any();
        };
        let root = workspace.root_of(session);
        let forwards = workspace
            .forwards
            .with(|_| tree_forwards(&workspace.forward_list(), root));
        if forwards.is_empty() {
            return view! {
                <p class=NOTE data-session-forwards-empty>"No agent in this session forwarded a web app or a port."</p>
            }
            .into_any();
        }
        forwards
            .into_iter()
            .map(|forward| forward_row(workspace, forward, menu))
            .collect_view()
            .into_any()
    };
    view! {
        <div class="flex flex-col gap-2" data-session-forwards>
            {body}
            {(!guest).then(|| view! {
                <div class="flex flex-wrap gap-2 pt-1">
                    <button type="button" class=BUTTON on:click=move |_| workspace.open_view(View::Forwards, None)>
                        "Open Forwards"
                    </button>
                </div>
            })}
        </div>
    }
}
