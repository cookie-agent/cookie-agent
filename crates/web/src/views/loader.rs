//! The open session's transcript while it loads whole: a skeleton of rows
//! under a line of how much has loaded, and a pill that counts its events
//! (`ClientDelivery::ReplayProgress`). A quick load shows nothing; once
//! shown, the skeleton stays long enough to read, then fades as the
//! transcript takes its place.

use std::time::Duration;

use cookie_agent_client_state::{ReplayCount, format::format_count};
use leptos::prelude::*;

use crate::workspace::Workspace;

/// How long a load runs before the skeleton shows.
const SHOW_AFTER: Duration = Duration::from_millis(150);
/// How long the skeleton shows at least, once it does.
const SHOWN_AT_LEAST: Duration = Duration::from_millis(400);
/// How long it takes to fade (`.loader-out`).
const FADE: Duration = Duration::from_millis(320);

/// Where the skeleton is.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Stage {
    Hidden,
    Shown,
    Fading,
}

/// The pill's words for `count`: the events loaded of how many, as far as
/// the daemon said.
fn count_label(count: Option<ReplayCount>) -> String {
    match count {
        Some(ReplayCount {
            events,
            total: Some(total),
        }) => format!(
            "{} / {} events",
            format_count(events.min(total)),
            format_count(total)
        ),
        Some(ReplayCount {
            events,
            total: None,
        }) => format!("{} events", format_count(events)),
        None => String::new(),
    }
}

/// How much of the session has loaded, from 0 to 1, when the daemon said
/// how much there is.
fn loaded(count: Option<ReplayCount>) -> Option<f64> {
    let ReplayCount { events, total } = count?;
    let total = total.filter(|total| *total > 0)?;
    #[allow(clippy::cast_precision_loss)]
    Some((events as f64 / total as f64).clamp(0.0, 1.0))
}

fn now() -> f64 {
    js_sys::Date::now()
}

#[component]
pub fn TranscriptLoader(workspace: Workspace) -> impl IntoView {
    let loading = Memo::new(move |_| {
        !workspace.new_session.get()
            && workspace.selected.get().is_some_and(|session_id| {
                workspace
                    .opening
                    .with(|opening| opening.contains(&session_id))
            })
    });
    let count = Memo::new(move |_| {
        let session_id = workspace.selected.get()?;
        workspace.with_store(|store| store.replay_counts.get(&session_id).copied())
    });
    // What the pill and the line last showed: the count goes with the
    // replay's end, while the skeleton still fades.
    let last = RwSignal::new(None::<ReplayCount>);
    Effect::new(move |_| {
        if let Some(count) = count.get() {
            last.set(Some(count));
        }
    });
    let stage = RwSignal::new(Stage::Hidden);
    let shown_at = StoredValue::new(0.0_f64);
    // Each change of `loading` outdates the timers set for the last one.
    let turn = StoredValue::new(0_u64);
    Effect::new(move |_| {
        let now_loading = loading.get();
        let this = turn.try_update_value(|turn| {
            *turn += 1;
            *turn
        });
        let current = move || turn.try_get_value() == this;
        if now_loading {
            if stage.get_untracked() == Stage::Fading {
                stage.set(Stage::Shown);
                return;
            }
            last.set(None);
            set_timeout(
                move || {
                    if current() && stage.get_untracked() == Stage::Hidden {
                        shown_at.set_value(now());
                        stage.set(Stage::Shown);
                    }
                },
                SHOW_AFTER,
            );
        } else if stage.get_untracked() == Stage::Shown {
            let shown_for = now() - shown_at.get_value();
            #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
            let wait = Duration::from_millis(
                (SHOWN_AT_LEAST.as_secs_f64() * 1000.0 - shown_for).max(0.0) as u64,
            );
            set_timeout(
                move || {
                    if !current() {
                        return;
                    }
                    stage.set(Stage::Fading);
                    set_timeout(
                        move || {
                            if current() {
                                stage.set(Stage::Hidden);
                            }
                        },
                        FADE,
                    );
                },
                wait,
            );
        }
    });
    // Once loaded, the line and the pill read whole while the skeleton
    // fades.
    let shown_count = move || {
        let last = last.get();
        if loading.get() {
            last
        } else {
            last.map(|count| ReplayCount {
                events: count.total.unwrap_or(count.events),
                ..count
            })
        }
    };
    view! {
        <Show when=move || stage.get() != Stage::Hidden>
            <div
                class="pointer-events-none absolute inset-0 overflow-hidden bg-surface"
                class:loader-out=move || stage.get() == Stage::Fading
                role="status"
                aria-label="Loading the session"
                data-transcript-loader
            >
                {move || match loaded(shown_count()) {
                    Some(part) => view! {
                        <div
                            class="loader-line absolute top-0 left-0 h-0.5 rounded-r-full bg-accent"
                            style=format!("width: {:.1}%", part * 100.0)
                        ></div>
                    }
                    .into_any(),
                    None => view! {
                        <div class="loader-line loader-line-unknown absolute top-0 left-0 h-0.5 rounded-full bg-accent"></div>
                    }
                    .into_any(),
                }}
                <div class="mx-auto flex max-w-[45rem] flex-col gap-3 px-3 py-2 sm:px-6" aria-hidden="true">
                    <Skeleton />
                </div>
                <div class="absolute inset-x-0 bottom-4 flex justify-center">
                    <div class="inline-flex items-center gap-2 rounded-full bg-raised py-1.5 pr-3 pl-2.5 text-xs text-muted shadow-menu">
                        <span class="size-3.5 animate-spin rounded-full border-2 border-line-2 border-t-accent"></span>
                        "Loading"
                        <span class="font-semibold text-fg tabular-nums" data-loader-count>
                            {move || count_label(shown_count())}
                        </span>
                    </div>
                </div>
            </div>
        </Show>
    }
}

/// Rows shaped like a transcript's: the pinned rows, a message, a reply
/// with its work and two calls, and its answer.
#[component]
fn Skeleton() -> impl IntoView {
    let line = |width: &'static str| {
        view! { <div class="skeleton h-2.5 rounded-md" style=format!("width: {width}")></div> }
    };
    let call = |height: &'static str, title: &'static str| {
        view! {
            <div class="flex flex-col gap-3 rounded-xl p-3.5 ring-1 ring-line" style=format!("height: {height}")>
                <div class="skeleton h-3" style=format!("width: {title}")></div>
                <div class="skeleton flex-1"></div>
            </div>
        }
    };
    view! {
        <div class="rise-in flex gap-2">
            <div class="skeleton h-6 w-40 max-w-[40%] rounded-full"></div>
            <div class="skeleton h-6 w-32 max-w-[35%] rounded-full"></div>
        </div>
        <div class="skeleton rise-in ml-auto h-11 w-[60%] rounded-2xl"></div>
        <div class="rise-in flex flex-col gap-2.5">
            <div class="flex items-center gap-2">
                <div class="skeleton size-[18px] rounded-[5px]"></div>
                <div class="skeleton h-2.5 w-28"></div>
            </div>
            {line("92%")}
            {line("74%")}
        </div>
        <div class="rise-in">{line("170px")}</div>
        <div class="rise-in">{call("11rem", "160px")}</div>
        <div class="rise-in">{call("7.25rem", "240px")}</div>
        <div class="rise-in flex flex-col gap-2.5">{line("88%")} {line("66%")} {line("52%")}</div>
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_count_never_passes_its_total() {
        let count = |events, total| Some(ReplayCount { events, total });
        // Each page also sends its last event, which the view may leave
        // out of its total.
        assert_eq!(loaded(count(1_400, Some(1_336))), Some(1.0));
        assert_eq!(loaded(count(668, Some(1_336))), Some(0.5));
        // Without a total there is nothing to measure against.
        assert_eq!(loaded(count(10, None)), None);
        assert_eq!(loaded(count(10, Some(0))), None);
        assert_eq!(loaded(None), None);
    }
}
