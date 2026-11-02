//! What is live in the open session, as badges in its title bar
//! (docs/site/specs/quiet-ink.md, D6): the goal, the calls running in the
//! background, the approvals waiting, and the context's use. A badge comes
//! when its state starts, follows it, and goes when it ends; a click opens
//! the session panel at its tab. The context ring always shows once a
//! request has measured it.

use cookie_agent_client_state::{
    format::{compact_count, format_count},
    rows::goal_bar_shows,
};
use cookie_agent_protocol::GoalStatus;
use leptos::prelude::*;

use crate::{
    commands::Command,
    icons::Glyph,
    workspace::{Panel, Workspace},
};

/// One badge: a quiet pill that tints on hover and settles in as it comes.
const BADGE: &str = "badge-in inline-flex h-11 lg:h-7 items-center gap-1.5 rounded-md px-2 text-xs font-medium tabular-nums text-fg-2 transition-colors hover:bg-hover hover:text-fg active:bg-press";

/// How much of `window` `tokens` fill, from 0 to 1.
fn filled(tokens: u64, window: Option<u64>) -> Option<f64> {
    let window = window.filter(|window| *window > 0)?;
    #[allow(clippy::cast_precision_loss)]
    Some((tokens as f64 / window as f64).clamp(0.0, 1.0))
}

/// The context ring's title.
pub(super) fn context_title(tokens: u64, window: Option<u64>) -> String {
    match window {
        Some(window) => format!(
            "Context: {} of {} tokens",
            format_count(tokens),
            format_count(window)
        ),
        None => format!(
            "Context: {} tokens in the last request",
            format_count(tokens)
        ),
    }
}

/// A ring `fraction` of the way round, in the accent.
fn ring(fraction: f64) -> impl IntoView {
    // r = 6: a circumference of 37.7.
    const ROUND: f64 = 37.7;
    view! {
        <svg class="h-3.5 w-3.5 -rotate-90" viewBox="0 0 16 16" aria-hidden="true">
            <circle cx="8" cy="8" r="6" fill="none" stroke="var(--color-line-2)" stroke-width="2.25"></circle>
            <circle
                cx="8"
                cy="8"
                r="6"
                fill="none"
                stroke="var(--color-accent)"
                stroke-width="2.25"
                stroke-linecap="round"
                stroke-dasharray=format!("{ROUND}")
                stroke-dashoffset=format!("{:.2}", ROUND * (1.0 - fraction))
            ></circle>
        </svg>
    }
}

/// The open session's live badges.
#[component]
pub fn LiveBadges(workspace: Workspace) -> impl IntoView {
    let goal = Memo::new(move |_| workspace.goal().filter(|goal| goal_bar_shows(goal.status)));
    let background = Memo::new(move |_| workspace.background_calls().len());
    let approvals = Memo::new(move |_| workspace.approvals().len());
    let context = move || {
        workspace.viewed.with(|viewed| {
            viewed
                .context_tokens
                .map(|tokens| (tokens, viewed.context_window))
        })
    };
    let goal_badge = move || {
        goal.get().map(|goal| {
            let paused = goal.status == GoalStatus::Paused;
            let done = goal.items.iter().filter(|item| item.finished).count();
            let total = goal.items.len();
            let objective = goal.objective.split_whitespace().collect::<Vec<_>>().join(" ");
            let title = if paused {
                format!("Goal (paused): {objective}")
            } else if total > 0 {
                format!("Goal: {objective} ({done} of {total} done)")
            } else {
                format!("Goal: {objective}")
            };
            view! {
                <button
                    type="button"
                    class=BADGE
                    id="goal-badge"
                    data-goal-status=if paused { "paused" } else { "active" }
                    aria-label=title.clone()
                    title=title
                    on:click=move |_| workspace.panel.set(Some(Panel::Goal))
                >
                    <Glyph
                        icon=if paused { icondata::LuCirclePause } else { icondata::LuTarget }
                        class=if paused { "text-sm text-muted" } else { "text-sm text-accent" }
                    />
                    {(total > 0).then(|| view! { <span data-goal-progress>{format!("{done}/{total}")}</span> })}
                </button>
            }
        })
    };
    view! {
        <div class="flex shrink-0 items-center gap-0.5" data-live-badges>
            {goal_badge}
            <Show when=move || { background.get() > 0 }>
                <button
                    type="button"
                    class=BADGE
                    id="background-badge"
                    aria-label=move || format!("{} in the background", background.get())
                    title=move || format!("{} running in the background: click for them", background.get())
                    on:click=move |_| workspace.run_command(Command::Background)
                >
                    <Glyph icon=icondata::LuLoaderCircle class="animate-spin text-sm text-accent" />
                    <span>{move || background.get()}</span>
                </button>
            </Show>
            <Show when=move || { approvals.get() > 0 }>
                <button
                    type="button"
                    class=format!("{BADGE} bg-warn-soft text-warn hover:bg-warn-soft hover:text-warn")
                    id="approvals-badge"
                    aria-label=move || format!("{} waiting for you", super::approvals_label(approvals.get()))
                    title="Approvals waiting: click to answer"
                    on:click=move |_| workspace.panel.set(Some(Panel::Approvals))
                >
                    <Glyph icon=icondata::LuShield class="text-sm" />
                    <span>{move || approvals.get()}</span>
                </button>
            </Show>
            {move || context().map(|(tokens, window)| {
                let title = context_title(tokens, window);
                view! {
                    <button
                        type="button"
                        class=BADGE
                        id="context"
                        aria-label=title.clone()
                        title=title
                        on:click=move |_| workspace.run_command(Command::Usage)
                    >
                        {ring(filled(tokens, window).unwrap_or(0.0))}
                        <span class="text-muted">
                            {match filled(tokens, window) {
                                Some(fraction) => format!("{:.0}%", fraction * 100.0),
                                None => compact_count(tokens),
                            }}
                        </span>
                    </button>
                }
            })}
        </div>
    }
}

#[cfg(test)]
mod tests {
    use super::filled;

    #[test]
    fn the_ring_fills_by_the_share_of_a_known_window() {
        assert_eq!(filled(0, Some(400_000)), Some(0.0));
        assert_eq!(filled(100_000, Some(400_000)), Some(0.25));
        assert_eq!(filled(500_000, Some(400_000)), Some(1.0), "never past full");
        assert_eq!(filled(10, None), None, "no window, no share");
        assert_eq!(filled(10, Some(0)), None);
    }
}
