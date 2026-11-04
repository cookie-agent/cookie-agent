//! Share links in the web client (`docs/site/specs/share-links.md` section
//! 7.1): the share dialog, which makes, copies, changes, and revokes an
//! asset's link, and the Sharing view, which lists every live link. The
//! requests are `crate::workspace::shares`.

use cookie_agent_client_state::shares::ShareLifetime;
use cookie_agent_protocol::Share;
use leptos::prelude::*;

use super::{BUTTON, ERROR, NOTE, PRIMARY, empty_state, loading};
use crate::{icons::Glyph, workspace::Workspace};

const ROW_BUTTON: &str =
    "inline-flex min-h-9 items-center gap-1 rounded-md px-2 text-xs hover:bg-hover active:bg-press";
const SELECT: &str =
    "min-h-9 rounded-lg bg-surface px-2 text-sm shadow-[inset_0_0_0_1px_var(--color-line-2)]";

/// When a link stops working, in this browser's time zone.
fn expiry_text(share: &Share) -> String {
    share.expires_at.map_or_else(
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
            if listed.shares.is_empty() {
                return view! {
                    {off}
                    {empty_state(
                        icondata::LuLink,
                        "No share links. Share an asset from its card, or from the Assets view, to give it a public link.",
                    )}
                }
                .into_any();
            }
            let count = listed.shares.len();
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
            "Public links to assets: anyone with one can open its asset in a browser until it expires or you revoke it."
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
