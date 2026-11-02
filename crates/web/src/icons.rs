//! The page's icons: Lucide (ISC), and Font Awesome Free 6's brand logos
//! (CC BY 4.0), which Lucide does not draw, from the `icondata` crate.
//! Components show them through `leptos_icons`; the transcript, which
//! renders to HTML text, inlines the same SVG. Each icon
//! is a static the linker keeps only when the page names it.
//!
//! Icons inherit the text colour (`currentColor`) and are hidden from
//! assistive technology: the button or text beside them carries the name.

use cookie_agent_client_state::{ToolStatus, activity::SessionActivity};
use cookie_agent_protocol::{PermissionMode, SessionStatus, ToolHint};
pub use icondata::Icon;
use leptos::prelude::*;

use crate::theme::ThemePreference;

/// `icon` as an inline element, `class` sizing or colouring it.
#[component]
pub fn Glyph(
    #[prop(into)] icon: Signal<Icon>,
    #[prop(into, optional)] class: String,
) -> impl IntoView {
    view! {
        <span class=format!("inline-flex shrink-0 items-center {class}") aria-hidden="true">
            <leptos_icons::Icon icon=icon />
        </span>
    }
}

/// `icon` as SVG markup for HTML the page renders as text, `class` on the
/// `<svg>`: a `<use>` of the icon's symbol in the page's sprite, which
/// holds each icon's paths once however many transcript rows show it.
pub fn svg(icon: Icon, class: &str) -> String {
    format!(
        "<svg class=\"inline-block shrink-0 {class}\" aria-hidden=\"true\" width=\"1em\" height=\"1em\" fill=\"currentColor\"><use href=\"#{}\"></use></svg>",
        symbol(icon)
    )
}

thread_local! {
    /// The icons whose symbol is in the sprite.
    static SYMBOLS: std::cell::RefCell<std::collections::HashSet<&'static str>> =
        std::cell::RefCell::default();
}

/// The ID of `icon`'s symbol in the page's sprite, which gets the symbol
/// the first time the icon is asked for.
fn symbol(icon: Icon) -> String {
    use std::hash::BuildHasher as _;
    let id = format!(
        "icon-{:x}",
        foldhash::fast::FixedState::default().hash_one(icon.data)
    );
    if SYMBOLS.with(|symbols| symbols.borrow_mut().insert(icon.data)) {
        add_symbol(&id, icon);
    }
    id
}

/// Adds `icon`'s symbol to the sprite, a hidden `<svg>` at the end of the
/// page made with the first.
#[cfg(target_arch = "wasm32")]
fn add_symbol(id: &str, icon: Icon) {
    const SPRITE: &str = "icon-sprite";
    let Some(document) = web_sys::window().and_then(|window| window.document()) else {
        return;
    };
    let sprite = match document.get_element_by_id(SPRITE) {
        Some(sprite) => sprite,
        None => {
            let Some(body) = document.body() else {
                return;
            };
            let _ = body.insert_adjacent_html(
                "beforeend",
                &format!(
                    "<svg id=\"{SPRITE}\" aria-hidden=\"true\" style=\"position:absolute;width:0;height:0;overflow:hidden\"></svg>"
                ),
            );
            let Some(sprite) = document.get_element_by_id(SPRITE) else {
                return;
            };
            sprite
        }
    };
    let _ = sprite.insert_adjacent_html(
        "beforeend",
        &format!(
            "<symbol id=\"{id}\" viewBox=\"{}\"{}>{}</symbol>",
            icon.view_box.unwrap_or("0 0 512 512"),
            symbol_paint(icon),
            icon.data
        ),
    );
}

#[cfg(not(target_arch = "wasm32"))]
fn add_symbol(_: &str, _: Icon) {}

/// A symbol's own paint: a stroked icon (Lucide's) draws its outline in the
/// text colour and fills nothing; a filled one (a brand's) takes the
/// `<svg>`'s fill. The stroke is a little lighter than Lucide's 2, as the
/// components draw it (`main.css`).
#[cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]
fn symbol_paint(icon: Icon) -> String {
    if icon.stroke.is_none() {
        return String::new();
    }
    format!(
        " fill=\"{}\" stroke=\"currentColor\" stroke-width=\"1.75\" stroke-linecap=\"{}\" stroke-linejoin=\"{}\"",
        icon.fill.unwrap_or("none"),
        icon.stroke_linecap.unwrap_or("round"),
        icon.stroke_linejoin.unwrap_or("round"),
    )
}

/// The icon of a tool call's presentation hint, by the hint and never by
/// the tool's name; a hint this build does not know is generic.
pub fn hint_icon(hint: Option<&str>) -> Icon {
    match hint.unwrap_or(ToolHint::GENERIC) {
        ToolHint::SHELL => icondata::LuTerminal,
        ToolHint::FILE_READ => icondata::LuFileText,
        ToolHint::FILE_WRITE | ToolHint::FILE_EDIT => icondata::LuSquarePen,
        ToolHint::FETCH => icondata::LuGlobe,
        ToolHint::SEARCH => icondata::LuSearch,
        ToolHint::DELEGATE => icondata::LuBot,
        ToolHint::MESSAGE => icondata::LuMail,
        ToolHint::SKILL => icondata::LuWandSparkles,
        ToolHint::GOAL => icondata::LuTarget,
        ToolHint::FIGURE => icondata::LuImage,
        _ => icondata::LuWrench,
    }
}

/// An asset kind's icon.
pub fn asset_icon(kind: cookie_agent_client_state::assets::AssetKind) -> Icon {
    use cookie_agent_client_state::assets::AssetKind;
    match kind {
        AssetKind::Image => icondata::LuImage,
        AssetKind::Page => icondata::LuCode,
        AssetKind::Document => icondata::LuFileText,
        AssetKind::Media => icondata::LuFilm,
        AssetKind::Other => icondata::LuFile,
    }
}

/// How a status shows: its icon, its name, the colour classes, and whether
/// the icon turns while the thing runs.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct StatusMark {
    pub icon: Icon,
    pub label: &'static str,
    pub tone: &'static str,
    pub spins: bool,
}

impl StatusMark {
    /// The icon's classes: its colour, turning while it runs, and `extra`.
    pub fn class(self, extra: &str) -> String {
        let spin = if self.spins { "animate-spin" } else { "" };
        format!("{} {spin} {extra}", self.tone)
    }
}

const RUNNING: &str = "text-accent";
const DONE: &str = "text-ok";
const FAILED: &str = "text-bad";
const QUIET: &str = "text-muted";

/// A tool call's status.
pub fn tool_status(status: &ToolStatus, backgrounded: bool) -> StatusMark {
    let (icon, label, tone, spins) = match status {
        ToolStatus::Running if backgrounded => {
            (icondata::LuHourglass, "in the background", QUIET, false)
        }
        ToolStatus::Running => (icondata::LuLoaderCircle, "running", RUNNING, true),
        ToolStatus::Completed => (icondata::LuCircleCheck, "done", DONE, false),
        ToolStatus::Failed => (icondata::LuCircleX, "failed", FAILED, false),
        ToolStatus::Cancelled => (icondata::LuBan, "cancelled", QUIET, false),
        ToolStatus::Interrupted => (icondata::LuCirclePause, "interrupted", QUIET, false),
    };
    StatusMark {
        icon,
        label,
        tone,
        spins,
    }
}

/// A session's status in the session list.
pub fn session_status(status: SessionStatus) -> StatusMark {
    let (icon, label, tone, spins) = match status {
        SessionStatus::Running => (icondata::LuLoaderCircle, "running", RUNNING, true),
        SessionStatus::Idle => (icondata::LuCircle, "idle", QUIET, false),
        SessionStatus::Completed => (icondata::LuCircleCheck, "completed", DONE, false),
        SessionStatus::Failed => (icondata::LuCircleX, "failed", FAILED, false),
        SessionStatus::Cancelled => (icondata::LuBan, "cancelled", QUIET, false),
        SessionStatus::Interrupted => (icondata::LuCirclePause, "interrupted", QUIET, false),
        SessionStatus::Unknown => (icondata::LuCircleHelp, "unknown", QUIET, false),
    };
    StatusMark {
        icon,
        label,
        tone,
        spins,
    }
}

/// The mark a session's row shows before its title, if any: a turning
/// circle while it runs, a pause while it is held, a clock while it waits
/// on a producer (an active
/// goal, a background call, a subagent, or an external producer) to start
/// its next run, and a mark when its last run failed or was interrupted. A
/// finished, idle, or cancelled session has none, so its title starts at
/// the row's edge. Running outranks waiting, which outranks a problem.
pub fn row_status(activity: &SessionActivity) -> Option<StatusMark> {
    match activity {
        SessionActivity::Running => Some(session_status(SessionStatus::Running)),
        // A pause, as the held line in the transcript shows; red when its
        // runs kept failing.
        SessionActivity::Held(reason) => Some(StatusMark {
            icon: icondata::LuPause,
            label: "held",
            tone: match reason {
                cookie_agent_protocol::Open::Known(
                    cookie_agent_protocol::HeldReason::Failing { .. },
                ) => FAILED,
                _ => RUNNING,
            },
            spins: false,
        }),
        SessionActivity::Waiting(_) => Some(StatusMark {
            icon: icondata::LuClock,
            label: "waiting",
            tone: RUNNING,
            spins: false,
        }),
        SessionActivity::Problem(status @ (SessionStatus::Failed | SessionStatus::Interrupted)) => {
            Some(session_status(*status))
        }
        SessionActivity::Problem(_) | SessionActivity::None => None,
    }
}

/// [`row_status`] of a row that knows only its status, such as a
/// subagent's in the tree: subagents report no producer they wait on.
pub fn row_status_of(status: SessionStatus) -> Option<StatusMark> {
    row_status(&match status {
        SessionStatus::Running => SessionActivity::Running,
        SessionStatus::Failed | SessionStatus::Cancelled | SessionStatus::Interrupted => {
            SessionActivity::Problem(status)
        }
        SessionStatus::Idle | SessionStatus::Completed | SessionStatus::Unknown => {
            SessionActivity::None
        }
    })
}

/// The theme button's icon: the theme the page is in, or half a circle for
/// the system's.
pub fn theme_icon(preference: ThemePreference) -> Icon {
    match preference {
        ThemePreference::System => icondata::LuSunMoon,
        ThemePreference::Light => icondata::LuSun,
        ThemePreference::Dark => icondata::LuMoon,
    }
}

/// The permission mode's icon, a shield of its own for each mode (a
/// phone's bottom bar shows only the icon), and what the mode does.
pub fn permission_mode(mode: PermissionMode) -> (Icon, &'static str) {
    match mode {
        PermissionMode::AutoApprove => (
            icondata::LuShieldHalf,
            "the classifier approves safe calls and asks you about the rest",
        ),
        PermissionMode::AutoApproveN => (
            icondata::LuShield,
            "the classifier approves safe calls and rejects the rest without asking",
        ),
        PermissionMode::AutoApproveY => (
            icondata::LuShieldCheck,
            "the classifier approves safe calls and approves the rest without asking",
        ),
        PermissionMode::Ask => (
            icondata::LuUserCheck,
            "every call that needs approval asks you",
        ),
        PermissionMode::Yolo => (
            icondata::LuShieldAlert,
            "every call that would ask is approved",
        ),
    }
}

#[cfg(test)]
mod tests {
    use cookie_agent_client_state::ToolStatus;
    use cookie_agent_protocol::SessionStatus;

    use super::{SessionActivity, row_status, session_status, svg, tool_status};

    #[test]
    fn statuses_name_themselves_and_rows_mark_only_what_needs_attention() {
        for status in [
            ToolStatus::Running,
            ToolStatus::Completed,
            ToolStatus::Failed,
            ToolStatus::Cancelled,
            ToolStatus::Interrupted,
        ] {
            assert!(!tool_status(&status, false).label.is_empty(), "{status:?}");
        }
        assert_eq!(
            tool_status(&ToolStatus::Running, true).label,
            "in the background"
        );
        assert_eq!(tool_status(&ToolStatus::Failed, false).label, "failed");

        assert_eq!(session_status(SessionStatus::Completed).label, "completed");
        assert_eq!(session_status(SessionStatus::Unknown).label, "unknown");
        // Rows mark only what runs, what waits, and what went wrong.
        assert!(row_status(&SessionActivity::Running).is_some());
        assert!(
            row_status(&SessionActivity::Waiting(vec![
                cookie_agent_protocol::WaitingOn::Goal,
            ]))
            .is_some()
        );
        let held = row_status(&SessionActivity::Held(cookie_agent_protocol::Open::Known(
            cookie_agent_protocol::HeldReason::Stopped {},
        )))
        .expect("a held mark");
        assert_eq!(held.label, "held");
        assert!(row_status(&SessionActivity::Problem(SessionStatus::Failed)).is_some());
        assert!(row_status(&SessionActivity::Problem(SessionStatus::Interrupted)).is_some());
        assert!(row_status(&SessionActivity::Problem(SessionStatus::Cancelled)).is_none());
        assert!(row_status(&SessionActivity::None).is_none());
    }

    #[test]
    fn inline_svg_is_hidden_and_points_into_the_sprite() {
        let markup = svg(icondata::LuCheck, "text-xs");
        assert!(markup.contains("aria-hidden=\"true\""));
        // The paths are in the sprite, once, under one ID per icon.
        assert!(!markup.contains(icondata::LuCheck.data));
        assert_eq!(markup, svg(icondata::LuCheck, "text-xs"));
        assert_ne!(
            markup,
            svg(icondata::LuBan, "text-xs"),
            "two icons share a symbol"
        );
    }
}
