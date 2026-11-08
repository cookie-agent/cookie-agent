//! The media viewer's Details (`docs/site/specs/asset-opening.md` section
//! 4): beside the file on a wide window and a sheet from the bottom on a
//! narrow one, an asset's title, type and size, handle, where it lives, and
//! who saved it, with *Rename*, *Open the session*, and *Delete…*.

use cookie_agent_protocol::{Asset, AssetId, AssetStore, human_bytes};
use leptos::{prelude::*, task::spawn_local};

use crate::{icons::Glyph, layout::View, workspace::Workspace};

const ACTION: &str = "inline-flex min-h-9 items-center gap-1.5 rounded-lg border border-white/15 px-2.5 text-xs hover:bg-white/10";
const LINK: &str = "text-left underline decoration-dotted underline-offset-2 hover:text-white";

/// The Details of the asset `id` of `store`, read when they open and again
/// after a rename.
#[component]
pub(super) fn AssetDetails(workspace: Workspace, store: AssetStore, id: AssetId) -> impl IntoView {
    let found = RwSignal::new(None::<Result<Option<Asset>, String>>);
    Effect::new(move |_| {
        // Read again once a rename closes its dialog.
        if workspace.asset_renaming.with(Option::is_some) {
            return;
        }
        let store = store.clone();
        spawn_local(async move {
            let read = workspace.get_asset(store, id).await;
            found.try_set(Some(read));
        });
    });
    let body = move || match found.get() {
        None => view! { <p class="text-white/60">"Loading…"</p> }.into_any(),
        Some(Err(error)) => view! { <p class="text-white/60">{error}</p> }.into_any(),
        Some(Ok(None)) => {
            view! { <p class="text-white/60">"This asset was deleted."</p> }.into_any()
        }
        Some(Ok(Some(asset))) => details(workspace, asset).into_any(),
    };
    view! {
        <aside
            id="media-viewer-info"
            aria-label="Details"
            class="absolute inset-x-0 bottom-0 z-20 flex max-h-[45%] flex-col gap-3 overflow-y-auto rounded-t-2xl border-t border-white/10 bg-[#141416] p-4 text-[0.8125rem] lg:static lg:max-h-none lg:w-72 lg:shrink-0 lg:rounded-none lg:border-t-0 lg:border-l lg:bg-white/[0.03]"
            on:click=|event| event.stop_propagation()
        >
            {body}
        </aside>
    }
}

fn details(workspace: Workspace, asset: Asset) -> impl IntoView {
    let place = super::assets::place_of(workspace, &asset);
    let (saved, saved_session) = super::assets::saved_by(workspace, &asset);
    // Its row is "Saved".
    let saved = saved.trim_start_matches("saved ").to_owned();
    let lives_in = match place.session {
        Some((session, title)) => {
            let title = title.unwrap_or_else(|| "its session".to_owned());
            view! {
                {format!("{} · ", place.project)}
                <button type="button" class=LINK data-details-session on:click=move |_| open_session(workspace, session)>
                    {title}
                </button>
            }
            .into_any()
        }
        None if matches!(asset.store, AssetStore::Shared) => place.project.into_any(),
        None => format!("{} · project", place.project).into_any(),
    };
    let saved = match saved_session {
        Some(session) => view! {
            <button type="button" class=LINK on:click=move |_| open_session(workspace, session)>{saved}</button>
        }
        .into_any(),
        None => saved.into_any(),
    };
    let session = saved_session.or(match asset.store {
        AssetStore::Session { session } => Some(session),
        _ => None,
    });
    let (rename, delete) = (asset.clone(), asset.clone());
    view! {
        <h3 class="flex min-w-0 items-center gap-2 text-sm font-bold" id="media-viewer-info-title">
            <Glyph icon=crate::icons::asset_icon(cookie_agent_client_state::assets::AssetKind::of(&asset.media_type)) />
            <span class="min-w-0 wrap-anywhere">{asset.title.clone()}</span>
        </h3>
        <dl class="grid grid-cols-[5rem_1fr] gap-x-3 gap-y-1.5">
            <dt class="text-white/55">"Type"</dt>
            <dd class="min-w-0 wrap-anywhere">{format!("{} · {}", asset.media_type, human_bytes(asset.byte_length))}</dd>
            <dt class="text-white/55">"Handle"</dt>
            <dd class="font-mono text-xs select-all">{asset.handle.to_string()}</dd>
            <dt class="text-white/55">"Lives in"</dt>
            <dd class="min-w-0">{lives_in}</dd>
            <dt class="text-white/55">"Saved"</dt>
            <dd class="min-w-0">{saved}</dd>
        </dl>
        <div class="flex flex-wrap gap-1.5">
            <button
                type="button"
                class=ACTION
                data-details-action="rename"
                on:click=move |_| workspace.asset_renaming.set(Some((rename.clone(), rename.title.clone())))
            >
                <Glyph icon=icondata::LuPencil />
                "Rename"
            </button>
            {session.map(|session| view! {
                <button type="button" class=ACTION data-details-action="session" on:click=move |_| open_session(workspace, session)>
                    <Glyph icon=icondata::LuMessagesSquare />
                    "Open the session"
                </button>
            })}
            <button
                type="button"
                class=format!("{ACTION} border-bad/40 text-bad")
                data-details-action="delete"
                on:click=move |_| workspace.asset_deleting.set(Some(delete.clone()))
            >
                <Glyph icon=icondata::LuTrash2 />
                "Delete…"
            </button>
        </div>
    }
}

/// Closes the viewer and the asset page, and opens `session` in the chat.
fn open_session(workspace: Workspace, session: cookie_agent_protocol::SessionId) {
    workspace.media_viewer.set(None);
    workspace.asset_page.set(None);
    workspace.open_session(session);
    workspace.open_view(View::Chat, None);
}
