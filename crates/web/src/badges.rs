//! Where a session runs, as its row shows it: the badges of
//! `client_state::badges`, each machine in its stable hue (the `hue-*`
//! theme tokens in `style/main.css`), and the hue's dot that marks a
//! runtime, in its machine's colour, in the bottom bar and on the runtime
//! cards.

use cookie_agent_client_state::badges::{SessionBadge, badge_hue};
use leptos::prelude::*;

use crate::{icons::Glyph, machines::platform_icon};

/// A badge's frame: small, on one line, truncating its label.
const BADGE: &str = "inline-flex max-w-[9rem] items-center gap-1 rounded border px-1.5 text-[0.6875rem] leading-4 font-medium";

/// How much of the line each badge gives up when the line is short: the
/// machines and the workspaces shrink alike, a machine's badge down to its
/// icon, its padding, and three characters.
const SHRINKS_FIRST: &str = "shrink-[100]";
const MACHINE_MIN: &str = "min-w-[calc(3ch_+_1.75rem)]";

/// Each hue's badge: its colour as the text, a faint tint of it behind,
/// and a border between. Complete literals, for Tailwind to find.
const HUES: [&str; 8] = [
    "border-hue-0/40 bg-hue-0/10 text-hue-0 dark:border-hue-0-dark/40 dark:bg-hue-0-dark/15 dark:text-hue-0-dark",
    "border-hue-1/40 bg-hue-1/10 text-hue-1 dark:border-hue-1-dark/40 dark:bg-hue-1-dark/15 dark:text-hue-1-dark",
    "border-hue-2/40 bg-hue-2/10 text-hue-2 dark:border-hue-2-dark/40 dark:bg-hue-2-dark/15 dark:text-hue-2-dark",
    "border-hue-3/40 bg-hue-3/10 text-hue-3 dark:border-hue-3-dark/40 dark:bg-hue-3-dark/15 dark:text-hue-3-dark",
    "border-hue-4/40 bg-hue-4/10 text-hue-4 dark:border-hue-4-dark/40 dark:bg-hue-4-dark/15 dark:text-hue-4-dark",
    "border-hue-5/40 bg-hue-5/10 text-hue-5 dark:border-hue-5-dark/40 dark:bg-hue-5-dark/15 dark:text-hue-5-dark",
    "border-hue-6/40 bg-hue-6/10 text-hue-6 dark:border-hue-6-dark/40 dark:bg-hue-6-dark/15 dark:text-hue-6-dark",
    "border-hue-7/40 bg-hue-7/10 text-hue-7 dark:border-hue-7-dark/40 dark:bg-hue-7-dark/15 dark:text-hue-7-dark",
];

/// The grey of "no workspace".
const GREY: &str = "border-crust text-ink dark:border-smoke dark:text-sand";

/// Each hue's dot.
const DOTS: [&str; 8] = [
    "bg-hue-0 dark:bg-hue-0-dark",
    "bg-hue-1 dark:bg-hue-1-dark",
    "bg-hue-2 dark:bg-hue-2-dark",
    "bg-hue-3 dark:bg-hue-3-dark",
    "bg-hue-4 dark:bg-hue-4-dark",
    "bg-hue-5 dark:bg-hue-5-dark",
    "bg-hue-6 dark:bg-hue-6-dark",
    "bg-hue-7 dark:bg-hue-7-dark",
];

/// The colour classes of a badge of `hue`; grey without one.
pub fn hue_class(hue: Option<u8>) -> &'static str {
    hue.and_then(|hue| HUES.get(usize::from(hue)).copied())
        .unwrap_or(GREY)
}

/// A small dot marking the runtime `runtime_id` in the hue of `hue_key`,
/// its machine's name ([`cookie_agent_client_state::badges::hue_key`]).
pub fn runtime_dot(hue_key: &str, runtime_id: &str) -> impl IntoView + use<> {
    let colour = DOTS[usize::from(badge_hue(hue_key))];
    view! {
        <span
            class=format!("inline-block size-2 shrink-0 rounded-full {colour}")
            aria-hidden="true"
            data-runtime-dot=runtime_id.to_owned()
        ></span>
    }
}

/// One badge of a session row: a machine, or the workspaces after it as
/// plain text, the first to truncate.
pub fn badge_view(badge: SessionBadge) -> AnyView {
    let class = format!(
        "{BADGE} {MACHINE_MIN} {SHRINKS_FIRST} {}",
        hue_class(badge.hue())
    );
    let hue = badge.hue().map(|hue| hue.to_string());
    match badge {
        SessionBadge::Workspace(path) => view! {
            <span class=format!("min-w-[4ch] {SHRINKS_FIRST} truncate font-mono") data-badge="workspace">{path}</span>
        }
        .into_any(),
        SessionBadge::Machine { name, platform } => {
            let icon = platform_icon(platform.as_deref());
            view! {
                <span class=class data-badge="machine" data-hue=hue>
                    <Glyph icon=icon class="shrink-0 text-[0.625rem]" />
                    <span class="min-w-0 truncate">{name}</span>
                </span>
            }
            .into_any()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_hue_has_its_own_badge_and_dot() {
        for hue in 0..cookie_agent_client_state::badges::BADGE_HUES {
            let class = hue_class(Some(hue));
            assert!(class.contains(&format!("text-hue-{hue} ")), "{class}");
            assert!(
                class.contains(&format!("dark:text-hue-{hue}-dark")),
                "{class}"
            );
            assert!(DOTS[usize::from(hue)].contains(&format!("bg-hue-{hue} ")));
        }
        assert_eq!(hue_class(None), GREY);
        assert_eq!(hue_class(Some(8)), GREY);
    }
}
