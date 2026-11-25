//! What is live in the open session, as badges in its title bar
//! (docs/site/specs/quiet-ink.md, D6): the goal, the calls running in the
//! background, the approvals waiting, and the context's use. A badge comes
//! when its state starts, follows it, and goes when it ends; a click opens
//! the session panel at its tab. The context ring always shows once a
//! request has measured it. A clock counts down to the next fire of a cron
//! job into the session (docs/site/specs/session-schedules.md, D1).

use cookie_agent_client_state::{
    cache::{CacheBadge, CacheHeat, badge_text, detail_lines},
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

/// What the cache badge shows: a value, so the badge's button is built once
/// while it shows and only its parts follow the clock.
#[derive(Clone, PartialEq)]
struct CacheLook {
    title: String,
    glyph: (icondata::Icon, &'static str, bool),
    text: String,
    planned: bool,
    cooling: bool,
}

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

/// The cache badge's icon and its colour (`docs/site/specs/cache-aware-idle.md`
/// section 4.5): a flame while warm, graded by heat; a snowflake once
/// expired; a spinner while compacting.
fn cache_icon(badge: &CacheBadge) -> (icondata::Icon, &'static str, bool) {
    match badge {
        CacheBadge::Compacting => (icondata::LuLoaderCircle, "text-sm text-accent", true),
        CacheBadge::InUse { .. } | CacheBadge::Forever { .. } => {
            (icondata::LuFlame, "text-sm text-hot", false)
        }
        CacheBadge::Counting { heat, .. } => match heat {
            CacheHeat::Hot => (icondata::LuFlame, "text-sm text-hot", false),
            CacheHeat::Warm => (icondata::LuFlame, "text-sm text-hot opacity-60", false),
            CacheHeat::Cooling => (icondata::LuFlame, "text-sm text-muted", false),
            CacheHeat::Expired => (icondata::LuSnowflake, "text-sm text-faint", false),
        },
    }
}

/// The open session's live badges.
#[component]
pub fn LiveBadges(workspace: Workspace) -> impl IntoView {
    // The cache badge counts down while it shows.
    let now = RwSignal::new(jiff::Timestamp::now());
    let ticker = set_interval_with_handle(
        move || now.set(jiff::Timestamp::now()),
        std::time::Duration::from_secs(1),
    )
    .ok();
    on_cleanup(move || {
        if let Some(ticker) = ticker {
            ticker.clear();
        }
    });
    // Each badge's look is a value, and its button is built once while it
    // shows: the clock ticks every second and the session's state with each
    // event, and a button built again with them would replay its entrance,
    // drop its hover and tooltip, and could lose a click pressed across a
    // tick.
    let cache = Memo::new(move |_| {
        let viewed = workspace.viewed.get();
        let clock = viewed.cache_clock?;
        let session = workspace.selected.get()?;
        let plan = workspace.sessions.with(|sessions| {
            sessions
                .iter()
                .find(|meta| meta.session_id == session)
                .and_then(|meta| meta.cache_compaction.clone())
        });
        let now = now.get();
        let badge = clock.badge(now, viewed.running, plan.as_ref());
        let zone = crate::cron::zone_at(now);
        let title = detail_lines(&clock, &badge, plan.as_ref(), now, &zone).join("\n");
        let (icon, class, spins) = cache_icon(&badge);
        Some(CacheLook {
            title,
            glyph: (icon, class, spins),
            text: badge_text(&badge),
            planned: matches!(badge, CacheBadge::Counting { planned: true, .. }),
            cooling: matches!(
                badge,
                CacheBadge::Counting {
                    heat: CacheHeat::Cooling,
                    ..
                }
            ),
        })
    });
    let cache_on =
        move |read: fn(&CacheLook) -> bool| cache.with(|look| look.as_ref().is_some_and(read));
    let cache_glyph = Memo::new(move |_| cache.with(|look| look.as_ref().map(|look| look.glyph)));
    let cache_badge = view! {
        <Show when=move || cache_on(|_| true)>
            <button
                type="button"
                class=format!("{BADGE} relative")
                id="cache-badge"
                aria-label=move || cache.with(|look| look.as_ref().map(|look| look.title.clone()))
                title=move || cache.with(|look| look.as_ref().map(|look| look.title.clone()))
                on:click=move |_| workspace.run_command(Command::Usage)
            >
                {move || cache_glyph.get().map(|(icon, class, spins)| view! {
                    <Glyph icon=icon class=if spins { format!("{class} animate-spin") } else { class.to_owned() } />
                })}
                <Show when=move || cache_on(|look| !look.text.is_empty())>
                    <span class=move || if cache_on(|look| look.cooling) { "text-warn" } else { "text-muted" }>
                        {move || cache.with(|look| look.as_ref().map(|look| look.text.clone()))}
                    </span>
                </Show>
                <Show when=move || cache_on(|look| look.planned)>
                    <span class="absolute right-1 top-1 h-1.5 w-1.5 rounded-full bg-accent" data-cache-planned></span>
                </Show>
            </button>
        </Show>
    };
    let goal = Memo::new(move |_| workspace.goal().filter(|goal| goal_bar_shows(goal.status)));
    let background = Memo::new(move |_| workspace.background_calls().len());
    let approvals = Memo::new(move |_| workspace.approvals().len());
    let context = Memo::new(move |_| {
        workspace.viewed.with(|viewed| {
            viewed
                .context_tokens
                .map(|tokens| (tokens, viewed.context_window))
        })
    });
    let context_title_now = move || {
        context
            .get()
            .map(|(tokens, window)| context_title(tokens, window))
    };
    // The next fire into the open session, or a fire waiting there.
    let fire = Memo::new(move |_| {
        if workspace.new_session.get() {
            return None;
        }
        let session = workspace.selected.get()?;
        let next = workspace.crons.with(|_| {
            cookie_agent_client_state::session_schedules::next_fire(session, &workspace.cron_jobs())
        })?;
        let now = now.get();
        let text = cookie_agent_client_state::session_schedules::next_fire_text(&next, now);
        let mut lines: Vec<String> = Vec::new();
        if next.queued {
            lines.push("A fire waits for the running turn to end.".to_owned());
        }
        lines.extend(next.each.iter().map(|(title, at)| match at {
            Some(at) => format!(
                "{title}: next fire {}",
                cookie_agent_client_state::cron::until(*at, now)
            ),
            None => format!("{title}: no next fire"),
        }));
        lines.push("Click for the session's schedules.".to_owned());
        let title = lines.join("\n");
        // Due within the minute, or waiting: in the accent.
        let soon = next.queued
            || next
                .at
                .is_some_and(|at| at.as_second() - now.as_second() < 60);
        Some((title, text, soon))
    });
    let fire_soon = Memo::new(move |_| fire.with(|fire| fire.as_ref().is_some_and(|fire| fire.2)));
    let fire_badge = view! {
        <Show when=move || fire.with(Option::is_some)>
            <button
                type="button"
                class=BADGE
                id="schedule-badge"
                aria-label=move || fire.with(|fire| fire.as_ref().map(|fire| fire.0.clone()))
                title=move || fire.with(|fire| fire.as_ref().map(|fire| fire.0.clone()))
                on:click=move |_| workspace.run_command(Command::Schedules)
            >
                {move || view! {
                    <Glyph icon=icondata::LuAlarmClock class=if fire_soon.get() { "text-sm text-accent" } else { "text-sm text-muted" } />
                }}
                <span class:text-accent=move || fire_soon.get() data-schedule-next>
                    {move || fire.with(|fire| fire.as_ref().map(|fire| fire.1.clone()))}
                </span>
            </button>
        </Show>
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
            {cache_badge}
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
                    // A word beside the count where there is room
                    // (docs/site/specs/web-polish.md, P3).
                    <span>{move || background.get()}<span class="hidden @3xl/chat:inline">" running"</span></span>
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
                    <span>
                        {move || approvals.get()}
                        <span class="hidden @3xl/chat:inline">{move || if approvals.get() == 1 { " approval" } else { " approvals" }}</span>
                    </span>
                </button>
            </Show>
            {fire_badge}
            <Show when=move || context.with(Option::is_some)>
                <button
                    type="button"
                    class=BADGE
                    id="context"
                    aria-label=context_title_now
                    title=context_title_now
                    on:click=move |_| workspace.run_command(Command::Usage)
                >
                    {move || context.get().map(|(tokens, window)| ring(filled(tokens, window).unwrap_or(0.0)))}
                    <span class="text-muted">
                        {move || context.get().map(|(tokens, window)| match filled(tokens, window) {
                            Some(fraction) => format!("{:.0}%", fraction * 100.0),
                            None => compact_count(tokens),
                        })}
                    </span>
                </button>
            </Show>
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
