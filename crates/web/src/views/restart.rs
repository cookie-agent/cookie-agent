//! Restarting the daemon: the banner while its configuration waits for a
//! restart or while a restart waits, and the confirmation that offers a
//! restart when idle or now. The calls are `crate::workspace::restart`.

use cookie_agent_client_state::restart::{
    RESTART_WHEN_IDLE, restart_explanation, restart_now_label, running_turns,
};
use cookie_agent_protocol::RestartWhen;
use leptos::prelude::*;

use super::{BUTTON, PRIMARY};
use crate::{icons::Glyph, workspace::Workspace};

/// "Restart needed to apply: plugins · Restart daemon", or, once a restart
/// is scheduled, "Restarting when idle…".
#[component]
pub(crate) fn RestartBanner(workspace: Workspace) -> impl IntoView {
    let restarting = move || workspace.restarting.get();
    let needed = move || workspace.restart_banner();
    view! {
        {move || {
            if let Some(text) = restarting() {
                return Some(
                    view! {
                        <div
                            id="restart-banner"
                            role="status"
                            data-restarting
                            class="flex shrink-0 items-center gap-2 border-b border-caramel bg-toasted px-3 py-2 text-sm dark:border-maple dark:bg-ember"
                        >
                            <Glyph icon=icondata::FaArrowsRotateSolid class="animate-spin text-caramel dark:text-maple" />
                            <span class="min-w-0 flex-1">{text}</span>
                        </div>
                    }
                    .into_any(),
                );
            }
            needed().map(|text| {
                view! {
                    <div
                        id="restart-banner"
                        role="region"
                        aria-label="Restart needed"
                        class="flex shrink-0 flex-wrap items-center gap-2 border-b border-toasted bg-parchment py-1 pr-1 pl-3 text-sm dark:border-ember dark:bg-char"
                    >
                        <Glyph icon=icondata::FaTriangleExclamationSolid class="text-caramel dark:text-maple" />
                        <span class="min-w-0 flex-1 py-2" id="restart-needed">{text}</span>
                        <button
                            type="button"
                            id="restart-banner-button"
                            class=BUTTON
                            on:click=move |_| workspace.ask_restart()
                        >
                            "Restart daemon"
                        </button>
                    </div>
                }
                .into_any()
            })
        }}
    }
}

/// The confirmation: when idle (the default) or now, naming the turns a
/// restart now interrupts. A sheet at the bottom of a phone, a centered
/// dialog elsewhere.
#[component]
pub(crate) fn RestartDialog(workspace: Workspace) -> impl IntoView {
    let running = move || workspace.sessions.with(|sessions| running_turns(sessions));
    let close = move || workspace.restart_confirm.set(false);
    view! {
        <Show when=move || workspace.restart_confirm.get()>
            <div class="fixed inset-0 z-40 bg-espresso/30 dark:bg-black/50" on:click=move |_| close()></div>
            <div
                id="restart-dialog"
                role="alertdialog"
                aria-modal="true"
                aria-labelledby="restart-title"
                class="fixed inset-x-0 bottom-0 z-50 flex max-h-[85vh] flex-col gap-3 overflow-y-auto rounded-t-xl border border-toasted bg-cream p-4 pb-[max(1rem,env(safe-area-inset-bottom))] text-sm shadow-lg sm:inset-x-auto sm:top-1/2 sm:bottom-auto sm:left-1/2 sm:w-[28rem] sm:max-w-[calc(100vw-2rem)] sm:-translate-x-1/2 sm:-translate-y-1/2 sm:rounded-xl dark:border-ember dark:bg-night"
                on:keydown=move |event| {
                    if event.key() == "Escape" {
                        close();
                    }
                }
            >
                <p id="restart-title" class="font-bold">"Restart the daemon?"</p>
                <p class="text-ink dark:text-sand" id="restart-explanation">{move || restart_explanation(running())}</p>
                <div class="flex flex-col gap-2 sm:flex-row-reverse sm:flex-wrap">
                    <button
                        type="button"
                        id="restart-idle"
                        class=PRIMARY
                        autofocus
                        on:click=move |_| workspace.restart(RestartWhen::Idle)
                    >
                        {RESTART_WHEN_IDLE}
                    </button>
                    <button
                        type="button"
                        id="restart-now"
                        class=BUTTON
                        on:click=move |_| workspace.restart(RestartWhen::Now)
                    >
                        {move || restart_now_label(running())}
                    </button>
                    <button type="button" id="restart-cancel" class=BUTTON on:click=move |_| close()>
                        "Cancel"
                    </button>
                </div>
            </div>
        </Show>
    }
}
