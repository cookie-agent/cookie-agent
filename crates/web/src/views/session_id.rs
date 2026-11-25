//! The open session's ID, small and quiet, which copies on a click: in the
//! title bar after the title and beside the session panel's heading, and,
//! for a subagent, its handle beside it. Other sessions and the CLI reach a
//! root session by its full ID; agents in a tree message a subagent by its
//! handle.

use cookie_agent_protocol::{SessionId, SessionOrigin};
use leptos::prelude::*;

use crate::{icons::Glyph, workspace::Workspace};

/// The open session's ID, and its handle when it is a subagent's.
pub(super) fn open_ids(workspace: Workspace) -> Option<(SessionId, Option<String>)> {
    if workspace.new_session.get() {
        return None;
    }
    let id = workspace.selected.get()?;
    let handle = workspace.tree.with(|tree| {
        let tree = tree.as_ref()?;
        let open = *crate::tree::path(tree, id).last()?;
        matches!(open.origin, SessionOrigin::Delegated { .. })
            .then(|| open.short_id.clone())
            .flatten()
    });
    Some((id, handle))
}

/// Copies `text`, saying `what` was copied.
fn copy(workspace: Workspace, what: &'static str, text: String, done: RwSignal<bool>) {
    leptos::task::spawn_local(async move {
        if crate::browser::copy_text(&text).await {
            workspace.status.set(format!("{what} copied."));
            done.set(true);
            set_timeout(
                move || done.set(false),
                std::time::Duration::from_millis(1400),
            );
        } else {
            workspace.status.set(
                "This browser does not let the page copy; select the text instead.".to_owned(),
            );
        }
    });
}

/// One value that copies on a click: mono, muted, a copy icon on hover, and
/// a check for a moment once copied. A long one gives way in its middle
/// (`01a1e57f…dfaf8acc`), its ends kept. `display` is its `display` class,
/// which may show it only where it fits.
fn copy_chip(
    workspace: Workspace,
    what: &'static str,
    value: String,
    attr: &'static str,
    display: &'static str,
) -> impl IntoView {
    let done = RwSignal::new(false);
    // The ends stay; the middle shrinks to an ellipsis first.
    let chars: Vec<char> = value.chars().collect();
    let (head, middle, tail) = if chars.len() > 20 {
        (
            chars[..8].iter().collect::<String>(),
            chars[8..chars.len() - 8].iter().collect::<String>(),
            chars[chars.len() - 8..].iter().collect::<String>(),
        )
    } else {
        (value.clone(), String::new(), String::new())
    };
    let copied = value.clone();
    view! {
        <button
            type="button"
            class=format!("group/id hit-area {display} h-6 min-w-0 shrink items-center gap-1 rounded-md px-1.5 font-mono text-[0.6875rem] text-muted transition-colors hover:bg-hover hover:text-fg")
            class:text-ok=move || done.get()
            title=format!("Click to copy the {}: {value}", what.to_lowercase())
            aria-label=format!("Copy the {}", what.to_lowercase())
            data-copy-id=attr
            data-copied=move || done.get().to_string()
            on:click=move |_| copy(workspace, what, copied.clone(), done)
        >
            <span class=move || if done.get() {
                "inline-flex shrink-0"
            } else {
                "inline-flex shrink-0 opacity-0 transition-opacity group-hover/id:opacity-100 group-focus-visible/id:opacity-100"
            }>
                <Glyph
                    icon=Signal::derive(move || if done.get() { icondata::LuCheck } else { icondata::LuCopy })
                    class="text-[0.75rem]"
                />
            </span>
            <span class="flex min-w-0">
                <span class="shrink-0">{head}</span>
                <span class="min-w-[1ch] shrink truncate">{middle}</span>
                <span class="shrink-0">{tail}</span>
            </span>
        </button>
    }
}

/// Where an ID chip shows inside a [`fitted_session_ids`] slot: from the
/// slot width its narrowest form takes (17 characters of 11 px JetBrains
/// Mono, its padding, and its icon: about 8.8rem), and the handle beside it
/// from the width both take with a handle of up to 20 characters (about
/// 18.9rem). The ID gives way before the handle does not, so a slot with
/// room for one keeps the ID.
const ID_FITS: &str = "hidden @min-[9rem]:inline-flex";
const HANDLE_FITS: &str = "hidden @min-[19rem]:inline-flex";

/// The open session's ID (and a subagent's handle before it), each copying
/// itself; nothing while no session is open. `class` places the group.
pub(super) fn session_ids(workspace: Workspace, class: &'static str) -> impl IntoView {
    ids_group(workspace, class, "inline-flex", "inline-flex")
}

/// [`session_ids`] in a slot of the room a row leaves it (a title bar's,
/// after the title): each chip shows only where it fits in its narrowest
/// form, and the slot clips, so the IDs never run under what follows them,
/// however crowded the row. `class` places the slot.
pub(super) fn fitted_session_ids(workspace: Workspace, class: &'static str) -> impl IntoView {
    view! {
        // A size container: its width comes from the row, not the IDs, and
        // the chips ask it whether they fit. The vertical padding keeps the
        // chips' 44 px touch inside what it clips.
        <div class=format!("@container -my-2.5 flex min-w-0 flex-1 basis-0 items-center overflow-hidden py-2.5 {class}")>
            {ids_group(workspace, "", HANDLE_FITS, ID_FITS)}
        </div>
    }
}

fn ids_group(
    workspace: Workspace,
    class: &'static str,
    handle_display: &'static str,
    id_display: &'static str,
) -> impl IntoView {
    // Drawn again only when the ID or the handle changes, not on every
    // change of the tree a running session makes.
    let ids = Memo::new(move |_| open_ids(workspace));
    move || {
        ids.get().map(|(id, handle)| {
            view! {
                <span class=format!("flex min-w-0 shrink-[4] items-center gap-0.5 {class}") data-session-ids>
                    {handle.map(|handle| copy_chip(workspace, "Handle", handle, "handle", handle_display))}
                    {copy_chip(workspace, "Session ID", id.to_string(), "session", id_display)}
                </span>
            }
        })
    }
}
