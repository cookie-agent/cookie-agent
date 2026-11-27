//! The GitHub star card (`docs/site/specs/github-star.md` section 2): over
//! the transcript's bottom right corner, the width of the transcript on a
//! phone. [`crate::workspace::StarPhase`] says what it shows.

use cookie_agent_protocol::{STAR_REPOSITORY_URL, StarAnswer};
use leptos::prelude::*;

use super::{BUTTON, ICON_BUTTON, PRIMARY};
use crate::{
    icons::Glyph,
    workspace::{StarPhase, Workspace},
};

/// **Star on GitHub ↗**: the repository's page in a new tab.
fn page_link(workspace: Workspace) -> impl IntoView {
    view! {
        <a
            data-star-open
            class=format!("{PRIMARY} flex-1")
            href=STAR_REPOSITORY_URL
            target="_blank"
            rel="noopener noreferrer"
            on:click=move |_| workspace.answer_star(StarAnswer::Opened)
        >
            <Glyph icon=icondata::LuStar />
            "Star on GitHub"
            <Glyph icon=icondata::LuExternalLink />
        </a>
    }
}

#[component]
pub(crate) fn StarCard(workspace: Workspace) -> impl IntoView {
    let phase = move || workspace.star.get();
    let body = move || match phase() {
        StarPhase::Hidden => ().into_any(),
        StarPhase::Ask { login } => {
            let star = match login {
                Some(login) => view! {
                    <button
                        type="button"
                        data-star-star
                        class=format!("{PRIMARY} flex-1")
                        on:click=move |_| workspace.answer_star(StarAnswer::Star)
                    >
                        <Glyph icon=icondata::LuStar />
                        {format!("Star as @{login}")}
                    </button>
                }
                .into_any(),
                None => page_link(workspace).into_any(),
            };
            view! {
                <button
                    type="button"
                    data-star-later
                    class=format!("{ICON_BUTTON} absolute top-1 right-1")
                    aria-label="Later"
                    title="Later"
                    on:click=move |_| workspace.answer_star(StarAnswer::Later)
                >
                    <Glyph icon=icondata::LuX />
                </button>
                <p id="star-title" class="flex items-center gap-2 pr-10 font-semibold">
                    <Glyph icon=icondata::LuStar class="text-accent" />
                    "Enjoying Cookie Agent?"
                </p>
                <p class="text-muted">"A star on GitHub helps others find it."</p>
                <div class="flex flex-wrap gap-1.5">
                    {star}
                    <button
                        type="button"
                        data-star-later
                        class=BUTTON
                        on:click=move |_| workspace.answer_star(StarAnswer::Later)
                    >
                        "Later"
                    </button>
                </div>
                <div class="-mt-1 -mr-1.5 -mb-2 flex justify-end">
                    <button
                        type="button"
                        data-star-dismiss
                        class="inline-flex min-h-9 items-center rounded-md px-2 text-xs text-muted hover:bg-hover hover:text-fg active:bg-press"
                        on:click=move |_| workspace.answer_star(StarAnswer::Dismiss)
                    >
                        "Don't remind me"
                    </button>
                </div>
            }
            .into_any()
        }
        StarPhase::Starring { login } => view! {
            <p id="star-title" class="flex items-center gap-2 font-semibold">
                <Glyph icon=icondata::LuStar class="text-accent" />
                "Enjoying Cookie Agent?"
            </p>
            <p class="text-muted">"A star on GitHub helps others find it."</p>
            <button type="button" class=PRIMARY disabled>
                <Glyph icon=icondata::LuLoaderCircle class="animate-spin" />
                {format!("Starring as @{login}…")}
            </button>
        }
        .into_any(),
        StarPhase::Thanked { opened } => view! {
            <p id="star-title" class="flex items-center gap-2 font-semibold" data-star-thanked>
                <Glyph icon=icondata::LuStar class="text-ok [&_svg]:fill-current" />
                {if opened { "Thanks!" } else { "Thanks for the star!" }}
            </p>
            {opened.then(|| view! { <p class="text-muted">"GitHub opened in a new tab."</p> })}
        }
        .into_any(),
        StarPhase::Failed { login } => view! {
            <p id="star-title" class="flex items-center gap-2 font-semibold" data-star-failed>
                <Glyph icon=icondata::LuCircleAlert class="text-accent" />
                {format!("Couldn't star as @{login}")}
            </p>
            <p class="text-muted">"Star it on GitHub instead?"</p>
            <div class="flex flex-wrap gap-1.5">
                {page_link(workspace)}
                <button type="button" class=BUTTON on:click=move |_| workspace.close_star()>
                    "Close"
                </button>
            </div>
        }
        .into_any(),
    };
    view! {
        <Show when=move || phase() != StarPhase::Hidden>
            <div
                id="star-card"
                role="dialog"
                aria-labelledby="star-title"
                class="absolute inset-x-2 bottom-3 z-20 flex flex-col gap-2.5 rounded-xl bg-raised p-3.5 text-sm ring-1 ring-line shadow-menu sm:inset-x-auto sm:right-4 sm:w-80"
            >
                {body}
            </div>
        </Show>
    }
}
