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
            <div class="fixed inset-0 z-40 bg-scrim dark:bg-black/50" on:click=move |_| close()></div>
            <div
                id="share-dialog"
                role="dialog"
                aria-modal="true"
                aria-labelledby="share-title"
                class="fixed inset-x-0 bottom-0 z-50 flex max-h-[85vh] flex-col gap-3 overflow-y-auto rounded-t-xl p-4 pb-[max(1rem,env(safe-area-inset-bottom))] text-sm sm:inset-x-auto sm:top-1/2 sm:bottom-auto sm:left-1/2 sm:w-[32rem] sm:max-w-[calc(100vw-2rem)] sm:-translate-x-1/2 sm:-translate-y-1/2 sm:rounded-xl bg-raised ring-1 ring-line shadow-menu"
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

/// How many guests a link has now, and how many it has had.
fn guests_text(share: &SessionShare) -> String {
    let views = format!(
        "{} view{}",
        share.views,
        if share.views == 1 { "" } else { "s" }
    );
    match share.connected {
        0 => views,
        1 => format!("{} is connected · {views}", share.label),
        count => format!("{count} connected as {} · {views}", share.label),
    }
}

/// The conversation share dialog, open while
/// `workspace.session_share_dialog` holds a session.
#[component]
pub(crate) fn SessionShareDialogView(workspace: Workspace) -> impl IntoView {
    let close = move || workspace.session_share_dialog.set(None);
    let dialog = move || workspace.session_share_dialog.get();
    view! {
        <Show when=move || dialog().is_some()>
            <div class="fixed inset-0 z-40 bg-scrim dark:bg-black/50" on:click=move |_| close()></div>
            <div
                id="session-share-dialog"
                role="dialog"
                aria-modal="true"
                aria-labelledby="session-share-title"
                class="fixed inset-x-0 bottom-0 z-50 flex max-h-[85vh] flex-col gap-3 overflow-y-auto rounded-t-xl p-4 pb-[max(1rem,env(safe-area-inset-bottom))] text-sm sm:inset-x-auto sm:top-1/2 sm:bottom-auto sm:left-1/2 sm:w-[32rem] sm:max-w-[calc(100vw-2rem)] sm:-translate-x-1/2 sm:-translate-y-1/2 sm:rounded-xl bg-raised ring-1 ring-line shadow-menu"
                on:keydown=move |event| {
                    if event.key() == "Escape" {
                        close();
                    }
                }
            >
                <div class="flex items-start gap-2">
                    <Glyph icon=icondata::LuShare2 class="mt-0.5 text-accent" />
                    <div class="flex min-w-0 flex-1 flex-col">
                        <p id="session-share-title" class="font-bold">"Share conversation"</p>
                        <p class="truncate text-muted">
                            {move || dialog().map(|dialog| dialog.title).unwrap_or_default()}
                        </p>
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
                <Show when=move || workspace.shares_look_private()>
                    <p class="flex items-start gap-2 rounded-lg bg-warn-soft px-3 py-2 text-[0.8125rem] text-warn">
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
                    let warning = view! {
                        <p class="flex items-start gap-2 rounded-lg bg-warn-soft px-3 py-2 text-[0.8125rem]" id="session-share-warning">
                            <Glyph icon=icondata::LuTriangleAlert class="mt-0.5 shrink-0 text-warn" />
                            <span>"Anyone with this link uses the session as you do: its tools, machines, and approvals, on your provider account."</span>
                        </p>
                    };
                    Some(match dialog.share.clone() {
                        None => view! {
                            <label class="flex flex-col gap-1 font-bold">
                                "For"
                                <input
                                    type="text"
                                    id="session-share-label"
                                    class="min-h-9 rounded-lg bg-surface px-3 font-normal shadow-[inset_0_0_0_1px_var(--color-line-2)]"
                                    placeholder="Guest"
                                    maxlength="30"
                                    prop:value=dialog.label.clone()
                                    on:input=move |event| {
                                        let label = event_target_value(&event);
                                        workspace.session_share_dialog.update(|dialog| {
                                            if let Some(dialog) = dialog {
                                                dialog.label = label;
                                            }
                                        });
                                    }
                                />
                            </label>
                            <label class="flex items-start gap-2">
                                <input
                                    type="checkbox"
                                    id="session-share-can-send"
                                    class="mt-1"
                                    prop:checked=dialog.can_send
                                    on:change=move |event| {
                                        let checked = event_target_checked(&event);
                                        workspace.session_share_dialog.update(|dialog| {
                                            if let Some(dialog) = dialog {
                                                dialog.can_send = checked;
                                            }
                                        });
                                    }
                                />
                                <span class="flex flex-col">
                                    "Can send messages"
                                    <span class=NOTE>"Off makes the link watch-only."</span>
                                </span>
                            </label>
                            {warning}
                            <div class="flex flex-wrap items-center gap-2">
                                <label class=NOTE for="session-share-lifetime">"Expires after"</label>
                                {lifetime_select(
                                    Some("session-share-lifetime"),
                                    Some(dialog.lifetime),
                                    move |lifetime| {
                                        workspace.session_share_dialog.update(|dialog| {
                                            if let Some(dialog) = dialog {
                                                dialog.lifetime = lifetime;
                                            }
                                        });
                                    },
                                )}
                                <span class="flex-1"></span>
                                <button
                                    type="button"
                                    id="session-share-create"
                                    class=PRIMARY
                                    disabled=dialog.busy
                                    on:click=move |_| workspace.create_session_share()
                                >
                                    <Glyph icon=icondata::LuLink />
                                    "Create link"
                                </button>
                            </div>
                        }
                        .into_any(),
                        Some(share) => {
                            let address = workspace.session_share_address(&share);
                            let copied = share.clone();
                            let revoked = share.id.clone();
                            let changed = share.id.clone();
                            let toggled = share.id.clone();
                            let can_send = share.can_send;
                            let fresh_label = share.label.clone();
                            view! {
                                <div class="flex min-w-0 items-center gap-2">
                                    <input
                                        type="text"
                                        readonly
                                        id="session-share-url"
                                        class="min-h-11 min-w-0 flex-1 rounded-lg bg-surface px-3 font-mono text-xs shadow-[inset_0_0_0_1px_var(--color-line-2)] lg:min-h-9"
                                        aria-label="The link"
                                        prop:value=address
                                    />
                                    <button
                                        type="button"
                                        id="session-share-copy"
                                        class=PRIMARY
                                        on:click=move |_| workspace.copy_session_share(&copied)
                                    >
                                        <Glyph icon=icondata::LuCopy />
                                        "Copy"
                                    </button>
                                </div>
                                <p class=NOTE id="session-share-meta">
                                    {format!("For {} · {} · {}", share.label, expiry_text_at(share.expires_at), guests_text(&share))}
                                </p>
                                <label class="flex items-start gap-2">
                                    <input
                                        type="checkbox"
                                        id="session-share-can-send"
                                        class="mt-1"
                                        prop:checked=can_send
                                        on:change=move |event| {
                                            workspace.update_session_share(SessionShareUpdateParams {
                                                id: toggled.clone(),
                                                label: None,
                                                can_send: Some(event_target_checked(&event)),
                                                expiry: None,
                                            });
                                        }
                                    />
                                    <span class="flex flex-col">
                                        "Can send messages"
                                        <span class=NOTE>"Off makes the link watch-only."</span>
                                    </span>
                                </label>
                                {warning}
                                <div class="flex flex-wrap items-center gap-2">
                                    {lifetime_select(
                                        Some("session-share-change-lifetime"),
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
                                        id="session-share-new"
                                        class=BUTTON
                                        title="Another link to this session, to revoke apart"
                                        on:click=move |_| {
                                            workspace.session_share_dialog.update(|dialog| {
                                                if let Some(dialog) = dialog {
                                                    dialog.label.clone_from(&fresh_label);
                                                    dialog.can_send = can_send;
                                                }
                                            });
                                            workspace.create_session_share();
                                        }
                                    >
                                        "New link"
                                    </button>
                                    <button
                                        type="button"
                                        id="session-share-revoke"
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
                        .map(|error| view! { <p class=ERROR id="session-share-error">{error}</p> })
                }}
            </div>
        </Show>
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
                {format!("{} · {}", expiry_text_at(share.expires_at), guests_text(&share))}
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
