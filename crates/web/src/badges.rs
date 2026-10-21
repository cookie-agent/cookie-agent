//! Where a session runs, as its row shows it: the badges of
//! `client_state::badges`, each machine in its stable hue (the `hue-*`
//! theme tokens in `style/main.css`), and the hue's dot that marks a
//! runtime, in its machine's colour, in the bottom bar and on the runtime
//! cards.

use cookie_agent_client_state::badges::{SessionBadge, badge_hue};
use leptos::prelude::*;

use crate::{icons::Glyph, machines::platform_icon};

/// A badge's frame: small, on one line, never cut: a machine's name always
/// shows whole ([`fit`] leaves out the badges that do not fit instead).
const BADGE: &str = "inline-flex shrink-0 items-center gap-1 whitespace-nowrap rounded border px-1.5 text-[0.6875rem] leading-4 font-medium";

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

/// One badge of a session row: a machine, whole, or the workspaces after
/// it as plain text, which shrink first, down to `…`.
pub fn badge_view(badge: SessionBadge) -> AnyView {
    let class = format!("{BADGE} {}", hue_class(badge.hue()));
    let hue = badge.hue().map(|hue| hue.to_string());
    match badge {
        SessionBadge::Workspace(path) => view! {
            <span class="min-w-[2ch] shrink truncate font-mono" data-badge="workspace">{path}</span>
        }
        .into_any(),
        SessionBadge::Machine { name, platform } => {
            let icon = platform_icon(platform.as_deref());
            view! {
                <span class=class data-badge="machine" data-hue=hue>
                    <Glyph icon=icon class="shrink-0 text-[0.625rem]" />
                    <span>{name}</span>
                </span>
            }
            .into_any()
        }
    }
}

/// A line's badges, then the `+N` badge [`fit`] shows for the machines
/// that do not fit.
pub fn badge_line(badges: Vec<SessionBadge>) -> impl IntoView {
    view! {
        {badges.into_iter().map(badge_view).collect_view()}
        <span class=format!("{BADGE} {GREY}") data-badge="more" hidden></span>
    }
}

/// Fits the badges of `line`, a one-line flex box: the workspaces shrink
/// first (by their style, down to `…`), then leave the line; when the
/// machines alone still do not fit, the last ones leave it, whole, for a
/// `+N` badge whose tooltip names them. A machine's name is never cut.
pub fn fit(line: &web_sys::Element) {
    use wasm_bindgen::JsCast as _;
    let elements = |selector: &str| {
        line.query_selector_all(selector)
            .map(|list| {
                (0..list.length())
                    .filter_map(|index| list.item(index))
                    .filter_map(|node| node.dyn_into::<web_sys::HtmlElement>().ok())
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default()
    };
    let machines = elements("[data-badge=machine]");
    let workspaces = elements("[data-badge=workspace]");
    let Some(more) = elements("[data-badge=more]").into_iter().next() else {
        return;
    };
    for badge in machines.iter().chain(&workspaces) {
        badge.set_hidden(false);
    }
    more.set_hidden(true);
    let fits = || line.scroll_width() <= line.client_width() + 1;
    if fits() {
        return;
    }
    // The workspaces, already down to `…`, go before any machine does:
    // they are the one machine's, so they would name a machine not shown.
    for workspace in &workspaces {
        workspace.set_hidden(true);
    }
    if fits() {
        return;
    }
    more.set_hidden(false);
    let mut left_out = Vec::new();
    for machine in machines.iter().rev() {
        machine.set_hidden(true);
        left_out.insert(0, machine.text_content().unwrap_or_default());
        more.set_text_content(Some(&format!("+{}", left_out.len())));
        more.set_title(&left_out.join(", "));
        if fits() {
            break;
        }
    }
}

/// Keeps `line` fitted ([`fit`]) whenever it changes size, for as long as
/// its owner lives; a caller fits it again when its badges change.
pub fn keep_fitted(line: NodeRef<leptos::html::Span>) {
    use wasm_bindgen::{JsCast as _, closure::Closure};
    let observer = StoredValue::new_local(None::<(web_sys::ResizeObserver, Closure<dyn Fn()>)>);
    Effect::new(move |_| {
        let Some(element) = line.get() else {
            return;
        };
        if observer.with_value(Option::is_some) {
            return;
        }
        let target: web_sys::Element = element.clone().into();
        fit(&target);
        let callback = Closure::<dyn Fn()>::new(move || fit(&target));
        if let Ok(watch) = web_sys::ResizeObserver::new(callback.as_ref().unchecked_ref()) {
            watch.observe(&element);
            observer.set_value(Some((watch, callback)));
        }
    });
    on_cleanup(move || {
        if let Some((watch, _)) = observer.try_update_value(Option::take).flatten() {
            watch.disconnect();
        }
    });
}

/// Fits `line` once the badges it is drawing now are on the page.
pub fn fit_soon(line: NodeRef<leptos::html::Span>) {
    request_animation_frame(move || {
        if let Some(element) = line.get_untracked() {
            fit(&element.into());
        }
    });
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
