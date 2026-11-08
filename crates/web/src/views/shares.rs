//! Share links in the web client (`docs/site/specs/share-links.md` section
//! 7.1): the share dialog, which makes, copies, changes, and revokes an
//! asset's link, and the Sharing view, which lists every live link. The
//! requests are `crate::workspace::shares`.

use cookie_agent_client_state::shares::ShareLifetime;
use cookie_agent_protocol::{SessionShare, SessionShareUpdateParams, Share};
use leptos::prelude::*;

use super::{BUTTON, ERROR, NOTE, PRIMARY, empty_state, loading};
use crate::{icons::Glyph, workspace::Workspace};

const ROW_BUTTON: &str =
    "inline-flex min-h-9 items-center gap-1 rounded-md px-2 text-xs hover:bg-hover active:bg-press";
const SELECT: &str =
    "min-h-9 rounded-lg bg-surface px-2 text-sm shadow-[inset_0_0_0_1px_var(--color-line-2)]";

/// When a link stops working, in this browser's time zone.
fn expiry_text(share: &Share) -> String {
    expiry_text_at(share.expires_at)
}

fn expiry_text_at(expires_at: Option<jiff::Timestamp>) -> String {
    expires_at.map_or_else(
        || "Never expires".to_owned(),
        |at| {
            format!(
                "Expires {}",
                at.to_zoned(jiff::tz::TimeZone::system())
                    .strftime("%b %-d, %Y, %H:%M")
            )
        },
    )
}

/// "3 views", and when the last was.
fn views_text(share: &Share) -> String {
    let views = format!(
        "{} view{}",
        share.views,
        if share.views == 1 { "" } else { "s" }
    );
    match share.last_viewed {
        Some(at) => format!(
            "{views}, last {}",
            at.to_zoned(jiff::tz::TimeZone::system())
                .strftime("%b %-d, %H:%M")
        ),
        None => views,
    }
}

/// The expiry choices, as a select whose change calls `chosen`: `selected`
/// chosen, or without one a "Change expiry…" prompt.
fn lifetime_select(
    id: Option<&'static str>,
    selected: Option<ShareLifetime>,
    chosen: impl Fn(ShareLifetime) + 'static,
) -> impl IntoView {
    view! {
        <select
            id=id
            class=SELECT
            aria-label="When the link expires"
            data-share-lifetime
            on:change=move |event| {
                if let Some(lifetime) = ShareLifetime::from_code(&event_target_value(&event)) {
                    chosen(lifetime);
                }
            }
        >
            {selected
                .is_none()
                .then(|| view! { <option value="" selected disabled>"Change expiry…"</option> })}
            {ShareLifetime::ALL
                .into_iter()
                .map(|lifetime| {
                    view! {
                        <option value=lifetime.code() selected=selected == Some(lifetime)>
                            {lifetime.label()}
                        </option>
                    }
                })
                .collect_view()}
        </select>
    }
}

/// The share dialog, open while `workspace.share_dialog` holds an asset. A
/// sheet at the bottom of a phone, a centered dialog elsewhere.
#[component]
pub(crate) fn ShareDialogView(workspace: Workspace) -> impl IntoView {
    let close = move || workspace.share_dialog.set(None);
    let dialog = move || workspace.share_dialog.get();
    view! {
        <Show when=move || dialog().is_some()>
            <div class="fixed inset-0 z-[65] bg-scrim dark:bg-black/50" on:click=move |_| close()></div>
            <div
                id="share-dialog"
                role="dialog"
                aria-modal="true"
                aria-labelledby="share-title"
                class="fixed inset-x-0 bottom-0 z-[70] flex max-h-[85vh] flex-col gap-3 overflow-y-auto rounded-t-xl p-4 pb-[max(1rem,env(safe-area-inset-bottom))] text-sm sm:inset-x-auto sm:top-1/2 sm:bottom-auto sm:left-1/2 sm:w-[32rem] sm:max-w-[calc(100vw-2rem)] sm:-translate-x-1/2 sm:-translate-y-1/2 sm:rounded-xl bg-raised ring-1 ring-line shadow-menu"
                on:keydown=move |event| {
                    if event.key() == "Escape" {
                        close();
                    }
                }
            >
                <div class="flex items-start gap-2">
                    <Glyph icon=icondata::LuShare2 class="mt-0.5 text-accent" />
                    <div class="flex min-w-0 flex-1 flex-col">
                        <p id="share-title" class="font-bold">"Share link"</p>
                        <p class="truncate text-muted" id="share-asset-title">
                            {move || dialog().map(|dialog| dialog.title).unwrap_or_default()}
                        </p>
                    </div>
                    <button
                        type="button"
                        class=super::ICON_BUTTON
                        aria-label="Close"
                        id="share-close"
                        on:click=move |_| close()
                    >
                        <Glyph icon=icondata::LuX />
                    </button>
                </div>
                {move || {
                    dialog()
                        .and_then(|dialog| dialog.saved_as)
                        .map(|handle| {
                            view! {
                                <p class=NOTE id="share-saved-as">
                                    {format!("Saved as {handle} in this session's assets, so the link can show it.")}
                                </p>
                            }
                        })
                }}
                <Show when=move || workspace.shares_look_private()>
                    <p class="flex items-start gap-2 rounded-lg bg-warn-soft px-3 py-2 text-[0.8125rem] text-warn" id="share-private">
                        <Glyph icon=icondata::LuTriangleAlert class="mt-0.5 shrink-0" />
                        <span>
                            "This link works only where cookie's page can be reached. "
                            <a
                                class="underline"
                                href="https://cookie-agent.github.io/cookie-agent/daemon/remote-access/#public-share-links"
                                target="_blank"
                                rel="noopener noreferrer"
                            >
                                "Set up a public address"
                            </a>
                        </span>
                    </p>
                </Show>
                {move || {
                    let dialog = dialog()?;
                    let busy = dialog.busy;
                    Some(match dialog.share.clone() {
                        None => view! {
                            <p class="text-muted">
                                "Anyone with the link can open this asset in a browser, without signing in, until it expires. It always shows the asset as it is now."
                            </p>
                            <div class="flex flex-wrap items-center gap-2">
                                <label class=NOTE for="share-lifetime">"Expires after"</label>
                                {lifetime_select(
                                    Some("share-lifetime"),
                                    Some(dialog.lifetime),
                                    move |lifetime| {
                                        workspace.share_dialog.update(|dialog| {
                                            if let Some(dialog) = dialog {
                                                dialog.lifetime = lifetime;
                                            }
                                        });
                                    },
                                )}
                                <span class="flex-1"></span>
                                <button
                                    type="button"
                                    id="share-create"
                                    class=PRIMARY
                                    disabled=busy
                                    on:click=move |_| workspace.create_share(false)
                                >
                                    <Glyph icon=icondata::LuLink />
                                    "Create link"
                                </button>
                            </div>
                        }
                        .into_any(),
                        Some(share) => {
                            let address = workspace.share_address(&share);
                            let copied = share.clone();
                            let revoked = share.id.clone();
                            let changed = share.id.clone();
                            view! {
                                <div class="flex min-w-0 items-center gap-2">
                                    <input
                                        type="text"
                                        readonly
                                        id="share-url"
                                        class="min-h-11 min-w-0 flex-1 rounded-lg bg-surface px-3 font-mono text-xs shadow-[inset_0_0_0_1px_var(--color-line-2)] lg:min-h-9"
                                        aria-label="The link"
                                        prop:value=address
                                        on:focus=move |event| {
                                            use wasm_bindgen::JsCast as _;
                                            if let Some(input) = event
                                                .target()
                                                .and_then(|target| target.dyn_into::<web_sys::HtmlInputElement>().ok())
                                            {
                                                input.select();
                                            }
                                        }
                                    />
                                    <button
                                        type="button"
                                        id="share-copy"
                                        class=PRIMARY
                                        on:click=move |_| workspace.copy_share(&copied)
                                    >
                                        <Glyph icon=icondata::LuCopy />
                                        "Copy"
                                    </button>
                                </div>
                                <p class=NOTE id="share-expiry">
                                    {format!("{} · {}", expiry_text(&share), views_text(&share))}
                                </p>
                                <div class="flex flex-wrap items-center gap-2">
                                    {lifetime_select(
                                        Some("share-change-lifetime"),
                                        None,
                                        move |lifetime| workspace.set_share_lifetime(changed.clone(), lifetime),
                                    )}
                                    <span class="flex-1"></span>
                                    <button
                                        type="button"
                                        id="share-new"
                                        class=BUTTON
                                        disabled=busy
                                        title="Another link to the same asset, to revoke apart"
                                        on:click=move |_| workspace.create_share(true)
                                    >
                                        "New link"
                                    </button>
                                    <button
                                        type="button"
                                        id="share-revoke"
                                        class=format!("{BUTTON} text-bad")
                                        on:click=move |_| workspace.revoke_share(revoked.clone())
                                    >
                                        "Revoke"
                                    </button>
                                </div>
                            }
                            .into_any()
                        }
                    })
                }}
                {move || {
                    dialog()
                        .and_then(|dialog| dialog.error)
                        .map(|error| view! { <p class=ERROR id="share-error">{error}</p> })
                }}
            </div>
        </Show>
    }
}

/// When a guest's link stops working, and what it allows, for the guest
/// page's top bar.
fn guest_bar_text(guest: &cookie_agent_protocol::GuestHello) -> String {
    let who = if guest.can_send {
        format!("Shared with you as {}", guest.label)
    } else {
        format!("Shared with you as {} · watch only", guest.label)
    };
    match guest.expires_at {
        Some(at) => format!(
            "{who} · link expires {}",
            at.to_zoned(jiff::tz::TimeZone::system()).strftime("%b %-d")
        ),
        None => who,
    }
}

/// The top bar of the guest page.
#[component]
pub(crate) fn GuestBar(workspace: Workspace) -> impl IntoView {
    move || {
        workspace.guest.get().map(|guest| {
            view! {
                <div id="guest-bar" class="flex min-w-0 items-center gap-2 border-b border-line px-3 py-2 text-xs text-muted">
                    <img src="icons/icon.svg" alt="" class="size-4 shrink-0" />
                    <span class="shrink-0 font-semibold">"cookie agent"</span>
                    <span class="min-w-0 truncate" data-guest-about>{guest_bar_text(&guest)}</span>
                </div>
            }
        })
    }
}

/// The title bar's badge of a session with a live link: how many guests are
/// connected, and a click opens the share dialog.
#[component]
pub(crate) fn LinkBadge(workspace: Workspace) -> impl IntoView {
    move || {
        if workspace.is_guest() || workspace.new_session.get() {
            return None;
        }
        let session = workspace.root_of(workspace.selected.get()?);
        let links = workspace.session_links(session);
        if links.is_empty() {
            return None;
        }
        let connected: u32 = links.iter().map(|link| link.connected).sum();
        let title = match connected {
            0 => "This conversation has a share link".to_owned(),
            1 => "A guest is connected through a share link".to_owned(),
            count => format!("{count} guests are connected through share links"),
        };
        Some(view! {
            <button
                type="button"
                id="session-link-badge"
                class="inline-flex min-h-9 shrink-0 items-center gap-1 rounded-full bg-accent-soft px-2 text-xs text-accent hover:bg-hover"
                title=title.clone()
                aria-label=title
                on:click=move |_| workspace.open_session_share_dialog_of(session)
            >
                <Glyph icon=icondata::LuLink />
                {(connected > 0).then(|| view! { <span data-guests-connected>{connected}</span> })}
            </button>
        })
    }
}

/// How many views a link has had, for its card.
fn link_meta(share: &SessionShare) -> String {
    format!(
        "{} · {} view{}",
        expiry_text_at(share.expires_at),
        share.views,
        if share.views == 1 { "" } else { "s" }
    )
}

/// Who is connected through a link now, for the pill on its card.
fn connected_text(connected: u32) -> Option<String> {
    match connected {
        0 => None,
        1 => Some("Connected".to_owned()),
        count => Some(format!("{count} connected")),
    }
}

/// A switch, on or off, for whether a link's guests can send.
fn send_switch(on: bool, label: &'static str, toggled: impl Fn(bool) + 'static) -> impl IntoView {
    let knob = if on {
        "absolute left-4 size-3.5 rounded-full bg-white transition-[left]"
    } else {
        "absolute left-0.5 size-3.5 rounded-full bg-white transition-[left]"
    };
    view! {
        <button
            type="button"
            role="switch"
            aria-checked=on.to_string()
            data-link-can-send
            class="hit-area inline-flex shrink-0 items-center gap-2 text-sm"
            on:click=move |_| toggled(!on)
        >
            <span
                class="relative inline-flex h-[1.125rem] w-[1.875rem] shrink-0 items-center rounded-full transition-colors"
                class:bg-accent=on
                class:bg-line-2=!on
            >
                <span class=knob></span>
            </span>
            {label}
        </button>
    }
}

/// One link of the conversation share panel.
fn link_card(
    workspace: Workspace,
    share: SessionShare,
    revoking: RwSignal<Option<cookie_agent_protocol::ShareId>>,
) -> impl IntoView {
    let address = workspace.session_share_address(&share);
    let copied = share.clone();
    let toggled = share.id.clone();
    let changed = share.id.clone();
    let asked = share.id.clone();
    let confirmed = share.id.clone();
    let this = share.id.clone();
    let confirming = move || revoking.with(|revoking| revoking.as_ref() == Some(&this));
    view! {
        <div class="flex flex-col gap-2 rounded-lg p-3 ring-1 ring-line" data-session-link=share.id.to_string()>
            <div class="flex min-w-0 items-center gap-2">
                <span class="min-w-0 truncate font-bold" data-link-label>{share.label.clone()}</span>
                {connected_text(share.connected).map(|text| view! {
                    <span class="inline-flex shrink-0 items-center gap-1 rounded-full bg-ok-soft px-2 text-xs text-ok" data-link-connected>
                        <span class="size-1.5 rounded-full bg-ok"></span>
                        {text}
                    </span>
                })}
                <span class="flex-1"></span>
                {send_switch(share.can_send, "Can send", move |can_send| {
                    workspace.update_session_share(SessionShareUpdateParams {
                        id: toggled.clone(),
                        label: None,
                        can_send: Some(can_send),
                        expiry: None,
                    });
                })}
            </div>
            <div class="flex min-w-0 items-center gap-2">
                <input
                    type="text"
                    readonly
                    data-link-url
                    class="min-h-11 min-w-0 flex-1 rounded-lg bg-surface px-3 font-mono text-xs shadow-[inset_0_0_0_1px_var(--color-line-2)] lg:min-h-9"
                    aria-label=format!("The link for {}", share.label)
                    prop:value=address
                    on:focus=move |event| {
                        use wasm_bindgen::JsCast as _;
                        if let Some(input) = event
                            .target()
                            .and_then(|target| target.dyn_into::<web_sys::HtmlInputElement>().ok())
                        {
                            input.select();
                        }
                    }
                />
                <button
                    type="button"
                    class=PRIMARY
                    data-link-copy
                    on:click=move |_| workspace.copy_session_share(&copied)
                >
                    <Glyph icon=icondata::LuCopy />
                    "Copy"
                </button>
            </div>
            <div class="flex flex-wrap items-center gap-2">
                <span class=format!("min-w-0 flex-1 {NOTE}") data-link-meta>{link_meta(&share)}</span>
                <Show
                    when=confirming
                    fallback=move || {
                        let changed = changed.clone();
                        let asked = asked.clone();
                        view! {
                            {lifetime_select(None, None, move |lifetime| {
                                workspace.update_session_share(SessionShareUpdateParams {
                                    id: changed.clone(),
                                    label: None,
                                    can_send: None,
                                    expiry: Some(lifetime.expiry(jiff::Timestamp::now())),
                                });
                            })}
                            <button
                                type="button"
                                class=format!("{ROW_BUTTON} text-bad")
                                data-link-revoke
                                on:click=move |_| revoking.set(Some(asked.clone()))
                            >
                                "Revoke"
                            </button>
                        }
                    }
                >
                    <span class="flex flex-wrap items-center gap-2 text-[0.8125rem]" data-link-revoke-confirm>
                        "Revoke this link? Its guests are disconnected."
                        <button
                            type="button"
                            class=format!("{BUTTON} text-bad")
                            data-link-revoke-yes
                            on:click={
                                let confirmed = confirmed.clone();
                                move |_| {
                                    revoking.set(None);
                                    workspace.revoke_share(confirmed.clone());
                                }
                            }
                        >
                            "Revoke"
                        </button>
                        <button type="button" class=BUTTON on:click=move |_| revoking.set(None)>"Keep"</button>
                    </span>
                </Show>
            </div>
        </div>
    }
}

/// The form of a new link. Its fields keep their own state, so nothing
/// redraws them while they are typed in.
fn new_link_form(workspace: Workspace, adding: RwSignal<bool>) -> impl IntoView {
    let label = RwSignal::new(String::new());
    let can_send = RwSignal::new(true);
    let lifetime = RwSignal::new(ShareLifetime::default());
    let busy = move || {
        workspace
            .session_share_dialog
            .with(|dialog| dialog.as_ref().is_some_and(|dialog| dialog.busy))
    };
    view! {
        <div class="flex flex-col gap-2 rounded-lg p-3 ring-1 ring-line ring-inset" id="session-share-form">
            <label class="flex flex-col gap-1 text-[0.8125rem] font-bold">
                "For"
                <input
                    type="text"
                    id="session-share-label"
                    class="min-h-9 rounded-lg bg-surface px-3 font-normal shadow-[inset_0_0_0_1px_var(--color-line-2)]"
                    placeholder="Guest"
                    maxlength="30"
                    prop:value=move || label.get()
                    on:input=move |event| label.set(event_target_value(&event))
                />
            </label>
            <div class="flex flex-wrap items-center gap-2">
                {move || send_switch(can_send.get(), "Can send messages", move |on| can_send.set(on))}
                <span class="flex-1"></span>
                {lifetime_select(
                    Some("session-share-lifetime"),
                    Some(lifetime.get_untracked()),
                    move |chosen| lifetime.set(chosen),
                )}
                <button
                    type="button"
                    id="session-share-create"
                    class=PRIMARY
                    disabled=busy
                    on:click=move |_| {
                        workspace.create_session_share(
                            label.get_untracked(),
                            can_send.get_untracked(),
                            lifetime.get_untracked(),
                            move || {
                                label.set(String::new());
                                adding.set(false);
                            },
                        );
                    }
                >
                    <Glyph icon=icondata::LuLink />
                    "Create link"
                </button>
            </div>
        </div>
    }
}

/// The conversation share panel, open while `workspace.session_share_dialog`
/// holds a session: every link of the session, and a form for another.
#[component]
pub(crate) fn SessionShareDialogView(workspace: Workspace) -> impl IntoView {
    let open = Memo::new(move |_| {
        workspace
            .session_share_dialog
            .with(|dialog| dialog.as_ref().map(|dialog| dialog.session_id))
    });
    move || {
        open.get().map(|session| {
            view! { <SessionSharePanel workspace=workspace session=session /> }
        })
    }
}

#[component]
fn SessionSharePanel(
    workspace: Workspace,
    session: cookie_agent_protocol::SessionId,
) -> impl IntoView {
    let close = move || workspace.session_share_dialog.set(None);
    let links = Memo::new(move |_| workspace.session_links(session));
    let adding = RwSignal::new(false);
    let revoking = RwSignal::new(None);
    let revoking_all = RwSignal::new(false);
    // The form shows by itself while there is no link yet: that is what the
    // panel was opened for. The list's first read says which.
    let listed = Memo::new(move |_| workspace.share_list.with(Option::is_some));
    let form_open = Memo::new(move |_| adding.get() || (listed.get() && links.with(Vec::is_empty)));
    let title = move || {
        workspace
            .session_share_dialog
            .with(|dialog| dialog.as_ref().map(|dialog| dialog.title.clone()))
            .unwrap_or_default()
    };
    let count = move || links.with(Vec::len);
    let several = move || count() >= 2;
    view! {
        <div class="fixed inset-0 z-40 bg-scrim dark:bg-black/50" on:click=move |_| close()></div>
        <div
            id="session-share-dialog"
            role="dialog"
            aria-modal="true"
            aria-labelledby="session-share-title"
            class="fixed inset-x-0 bottom-0 z-50 flex max-h-[85vh] flex-col overflow-hidden rounded-t-xl text-sm sm:inset-x-auto sm:top-1/2 sm:bottom-auto sm:left-1/2 sm:w-[34rem] sm:max-w-[calc(100vw-2rem)] sm:-translate-x-1/2 sm:-translate-y-1/2 sm:rounded-xl bg-raised ring-1 ring-line shadow-menu"
            on:keydown=move |event| {
                if event.key() == "Escape" {
                    close();
                }
            }
        >
            <div class="flex items-start gap-2 p-4 pb-2">
                <Glyph icon=icondata::LuShare2 class="mt-0.5 text-accent" />
                <div class="flex min-w-0 flex-1 flex-col">
                    <p id="session-share-title" class="font-bold">"Share conversation"</p>
                    <p class="truncate text-muted">{title}</p>
                </div>
                <button
                    type="button"
                    class=super::ICON_BUTTON
                    aria-label="Close"
                    id="session-share-close"
                    on:click=move |_| close()
                >
                    <Glyph icon=icondata::LuX />
                </button>
            </div>
            <div class="flex min-h-0 flex-col gap-3 overflow-y-auto px-4 pb-4">
                <Show when=move || workspace.shares_look_private()>
                    <p class="flex items-start gap-2 rounded-lg bg-warn-soft px-3 py-2 text-[0.8125rem] text-warn">
                        <Glyph icon=icondata::LuTriangleAlert class="mt-0.5 shrink-0" />
                        <span>
                            "These links work only where cookie's page can be reached. "
                            <a
                                class="underline"
                                href="https://cookie-agent.github.io/cookie-agent/daemon/remote-access/#public-share-links"
                                target="_blank"
                                rel="noopener noreferrer"
                            >
                                "Set up a public address"
                            </a>
                        </span>
                    </p>
                </Show>
                <p class="flex items-start gap-2 rounded-lg bg-warn-soft px-3 py-2 text-[0.8125rem]" id="session-share-warning">
                    <Glyph icon=icondata::LuTriangleAlert class="mt-0.5 shrink-0 text-warn" />
                    <span>"Anyone with a link that can send uses the session as you do: its tools, machines, and approvals, on your provider account."</span>
                </p>
                <Show when=move || count() != 0>
                    <p class=format!("font-bold {NOTE}") id="session-share-count">
                        {move || format!("{} live link{}", count(), if count() == 1 { "" } else { "s" })}
                    </p>
                </Show>
                {move || {
                    links
                        .get()
                        .into_iter()
                        .map(|share| link_card(workspace, share, revoking))
                        .collect_view()
                }}
                <Show
                    when=move || form_open.get()
                    fallback=move || view! {
                        <button
                            type="button"
                            id="session-share-add"
                            class=format!("{BUTTON} self-start")
                            on:click=move |_| adding.set(true)
                        >
                            <Glyph icon=icondata::LuPlus />
                            "New link"
                        </button>
                    }
                >
                    {new_link_form(workspace, adding)}
                </Show>
                {move || {
                    workspace.session_share_dialog.with(|dialog| {
                        dialog.as_ref().and_then(|dialog| dialog.error.clone())
                    })
                    .map(|error| view! { <p class=ERROR id="session-share-error">{error}</p> })
                }}
            </div>
            <Show when=move || count() != 0>
                <div class="flex flex-wrap items-center gap-2 border-t border-line px-4 py-2">
                    <button
                        type="button"
                        id="session-share-sharing-view"
                        class="text-[0.8125rem] text-muted hover:text-fg hover:underline"
                        on:click=move |_| {
                            close();
                            workspace.open_view(crate::layout::View::Sharing, None);
                        }
                    >
                        "All share links in the Sharing view"
                    </button>
                    <span class="flex-1"></span>
                    <Show when=several>
                        <Show
                            when=move || revoking_all.get()
                            fallback=move || view! {
                                <button
                                    type="button"
                                    id="session-share-revoke-all"
                                    class=format!("{ROW_BUTTON} text-bad")
                                    on:click=move |_| revoking_all.set(true)
                                >
                                    "Revoke all"
                                </button>
                            }
                        >
                            <span class="flex flex-wrap items-center gap-2 text-[0.8125rem]" id="session-share-revoke-all-confirm">
                                {move || format!("Revoke all {} links? Their guests are disconnected.", count())}
                                <button
                                    type="button"
                                    id="session-share-revoke-all-yes"
                                    class=format!("{BUTTON} text-bad")
                                    on:click=move |_| {
                                        revoking_all.set(false);
                                        workspace.revoke_session_links(session);
                                    }
                                >
                                    "Revoke all"
                                </button>
                                <button type="button" class=BUTTON on:click=move |_| revoking_all.set(false)>"Keep"</button>
                            </span>
                        </Show>
                    </Show>
                </div>
            </Show>
        </div>
    }
}

/// One conversation share in the Sharing view.
fn session_share_row(workspace: Workspace, share: SessionShare) -> impl IntoView {
    let address = workspace.session_share_address(&share);
    let copied = share.clone();
    let revoked = share.id.clone();
    let changed = share.id.clone();
    let session_id = share.session_id;
    let title = share
        .title
        .clone()
        .unwrap_or_else(|| "A conversation".to_owned());
    let kind = if share.can_send {
        "Can send"
    } else {
        "Watch only"
    };
    view! {
        <div class="flex flex-col gap-1 border-b border-line py-2" data-session-share-row=share.id.to_string()>
            <div class="flex min-w-0 items-center gap-2">
                <Glyph icon=icondata::LuMessagesSquare class="text-accent" />
                <button
                    type="button"
                    class="min-w-0 flex-1 truncate text-left text-sm font-bold hover:underline"
                    title="Open the share dialog"
                    on:click=move |_| workspace.open_session_share_dialog_of(session_id)
                >
                    {title}
                </button>
                <span class=NOTE>{format!("For {} · {kind}", share.label)}</span>
            </div>
            <p class="truncate font-mono text-xs text-muted select-all">{address}</p>
            <p class=NOTE>
                {connected_text(share.connected).map_or_else(
                    || link_meta(&share),
                    |connected| format!("{connected} · {}", link_meta(&share)),
                )}
            </p>
            <div class="flex flex-wrap items-center gap-1">
                <button
                    type="button"
                    class=ROW_BUTTON
                    data-share-action="copy"
                    on:click=move |_| workspace.copy_session_share(&copied)
                >
                    <Glyph icon=icondata::LuCopy />
                    "Copy"
                </button>
                {lifetime_select(
                    None,
                    None,
                    move |lifetime| workspace.update_session_share(SessionShareUpdateParams {
                        id: changed.clone(),
                        label: None,
                        can_send: None,
                        expiry: Some(lifetime.expiry(jiff::Timestamp::now())),
                    }),
                )}
                <span class="flex-1"></span>
                <button
                    type="button"
                    class=format!("{ROW_BUTTON} text-bad")
                    data-share-action="revoke"
                    on:click=move |_| workspace.revoke_share(revoked.clone())
                >
                    <Glyph icon=icondata::LuTrash2 />
                    "Revoke"
                </button>
            </div>
        </div>
    }
}

/// The Sharing view: every live link, newest first, with *Copy*, *Change
/// expiry*, and *Revoke*, and *Revoke all*.
pub fn sharing_page(workspace: Workspace) -> impl IntoView {
    Effect::new(move |_| workspace.refresh_shares());
    let list = move || match workspace.share_list.get() {
        None => loading("Listing share links…").into_any(),
        Some(Err(error)) => view! { <p class=ERROR>{error}</p> }.into_any(),
        Some(Ok(listed)) => {
            let off = (!listed.enabled).then(|| {
                view! {
                    <p class=ERROR id="shares-off">
                        "Share links are off on this daemon ([share] enabled = false): no link works until they are on again."
                    </p>
                }
            });
            if listed.shares.is_empty() && listed.sessions.is_empty() {
                return view! {
                    {off}
                    {empty_state(
                        icondata::LuLink,
                        "No share links. Share an asset from its card, or a conversation from its ⋮ menu, to give it a public link.",
                    )}
                }
                .into_any();
            }
            let count = listed.shares.len() + listed.sessions.len();
            view! {
                {off}
                <div class="flex flex-wrap items-center gap-2">
                    <p class=NOTE id="shares-count">
                        {format!("{count} live link{}", if count == 1 { "" } else { "s" })}
                    </p>
                    <span class="flex-1"></span>
                    <Show
                        when=move || workspace.share_revoking_all.get()
                        fallback=move || {
                            view! {
                                <button
                                    type="button"
                                    id="shares-revoke-all"
                                    class=format!("{BUTTON} text-bad")
                                    on:click=move |_| workspace.share_revoking_all.set(true)
                                >
                                    "Revoke all"
                                </button>
                            }
                        }
                    >
                        <span class="flex flex-wrap items-center gap-2" id="shares-revoke-all-confirm">
                            <span class="text-sm">"Revoke every link? They stop working at once."</span>
                            <button
                                type="button"
                                id="shares-revoke-all-yes"
                                class=format!("{BUTTON} border-bad text-bad")
                                on:click=move |_| workspace.revoke_all_shares()
                            >
                                "Revoke all"
                            </button>
                            <button
                                type="button"
                                class=BUTTON
                                on:click=move |_| workspace.share_revoking_all.set(false)
                            >
                                "Keep"
                            </button>
                        </span>
                    </Show>
                </div>
                <div class="flex flex-col">
                    {listed
                        .sessions
                        .into_iter()
                        .map(|share| session_share_row(workspace, share))
                        .collect_view()}
                    {listed
                        .shares
                        .into_iter()
                        .map(|share| share_row(workspace, share))
                        .collect_view()}
                </div>
            }
            .into_any()
        }
    };
    view! {
        <p class="text-sm text-muted">
            "Public links to assets and conversations: anyone with one can open it in a browser until it expires or you revoke it."
        </p>
        {list}
    }
}

/// Who made a link: the user, or an agent in a session.
fn made_by(share: &Share) -> String {
    let at = share
        .created
        .at
        .to_zoned(jiff::tz::TimeZone::system())
        .strftime("%b %-d, %H:%M")
        .to_string();
    match &share.created.agent {
        Some(agent) => format!("Made by {agent} on {at}"),
        None => format!("Made by you on {at}"),
    }
}

fn share_row(workspace: Workspace, share: Share) -> impl IntoView {
    let address = workspace.share_address(&share);
    let copied = share.clone();
    let revoked = share.id.clone();
    let changed = share.id.clone();
    let (store, asset, title) = (share.store.clone(), share.asset, share.title.clone());
    view! {
        <div class="flex flex-col gap-1 border-b border-line py-2" data-share-row=share.id.to_string()>
            <div class="flex min-w-0 items-center gap-2">
                <Glyph icon=icondata::LuLink class="text-accent" />
                <button
                    type="button"
                    class="min-w-0 flex-1 truncate text-left text-sm font-bold hover:underline"
                    data-share-title
                    title="Open the share dialog"
                    on:click=move |_| workspace.open_share_dialog(store.clone(), asset, title.clone())
                >
                    {share.title.clone()}
                </button>
                <span class=NOTE>
                    {format!("{} · {}", share.media_type, cookie_agent_protocol::human_bytes(share.byte_length))}
                </span>
            </div>
            <p class="truncate font-mono text-xs text-muted select-all" data-share-url>{address}</p>
            <p class=NOTE data-share-meta>
                {format!("{} · {} · {}", expiry_text(&share), views_text(&share), made_by(&share))}
            </p>
            <div class="flex flex-wrap items-center gap-1">
                <button
                    type="button"
                    class=ROW_BUTTON
                    data-share-action="copy"
                    on:click=move |_| workspace.copy_share(&copied)
                >
                    <Glyph icon=icondata::LuCopy />
                    "Copy"
                </button>
                {lifetime_select(
                    None,
                    None,
                    move |lifetime| workspace.set_share_lifetime(changed.clone(), lifetime),
                )}
                <span class="flex-1"></span>
                <button
                    type="button"
                    class=format!("{ROW_BUTTON} text-bad")
                    data-share-action="revoke"
                    on:click=move |_| workspace.revoke_share(revoked.clone())
                >
                    <Glyph icon=icondata::LuTrash2 />
                    "Revoke"
                </button>
            </div>
        </div>
    }
}
