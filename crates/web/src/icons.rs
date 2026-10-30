//! The page's icons: Font Awesome Free 6, whose artwork is CC BY 4.0, from
//! the `icondata` crate. Components show them through `leptos_icons`; the
//! transcript, which renders to HTML text, inlines the same SVG. Each icon
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
            "<symbol id=\"{id}\" viewBox=\"{}\">{}</symbol>",
            icon.view_box.unwrap_or("0 0 512 512"),
            icon.data
        ),
    );
}

#[cfg(not(target_arch = "wasm32"))]
fn add_symbol(_: &str, _: Icon) {}

/// The icon of a tool call's presentation hint, by the hint and never by
/// the tool's name; a hint this build does not know is generic.
pub fn hint_icon(hint: Option<&str>) -> Icon {
    match hint.unwrap_or(ToolHint::GENERIC) {
        ToolHint::SHELL => icondata::FaTerminalSolid,
        ToolHint::FILE_READ => icondata::FaFileLinesSolid,
        ToolHint::FILE_WRITE | ToolHint::FILE_EDIT => icondata::FaPenToSquareSolid,
        ToolHint::FETCH => icondata::FaGlobeSolid,
        ToolHint::SEARCH => icondata::FaMagnifyingGlassSolid,
        ToolHint::DELEGATE => icondata::FaRobotSolid,
        ToolHint::MESSAGE => icondata::FaEnvelopeSolid,
        ToolHint::SKILL => icondata::FaWandMagicSparklesSolid,
        ToolHint::GOAL => icondata::FaBullseyeSolid,
        _ => icondata::FaWrenchSolid,
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

const RUNNING: &str = "text-caramel dark:text-maple";
const DONE: &str = "text-green-700 dark:text-green-400";
const FAILED: &str = "text-red-700 dark:text-red-400";
const QUIET: &str = "text-ink dark:text-sand";

/// A tool call's status.
pub fn tool_status(status: &ToolStatus, backgrounded: bool) -> StatusMark {
    let (icon, label, tone, spins) = match status {
        ToolStatus::Running if backgrounded => (
            icondata::FaHourglassHalfSolid,
            "in the background",
            QUIET,
            false,
        ),
        ToolStatus::Running => (icondata::FaCircleNotchSolid, "running", RUNNING, true),
        ToolStatus::Completed => (icondata::FaCircleCheckSolid, "done", DONE, false),
        ToolStatus::Failed => (icondata::FaCircleXmarkSolid, "failed", FAILED, false),
        ToolStatus::Cancelled => (icondata::FaBanSolid, "cancelled", QUIET, false),
        ToolStatus::Interrupted => (icondata::FaCirclePauseSolid, "interrupted", QUIET, false),
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
        SessionStatus::Running => (icondata::FaCircleNotchSolid, "running", RUNNING, true),
        SessionStatus::Idle => (icondata::FaCircleRegular, "idle", QUIET, false),
        SessionStatus::Completed => (icondata::FaCircleCheckSolid, "completed", DONE, false),
        SessionStatus::Failed => (icondata::FaCircleXmarkSolid, "failed", FAILED, false),
        SessionStatus::Cancelled => (icondata::FaBanSolid, "cancelled", QUIET, false),
        SessionStatus::Interrupted => (icondata::FaCirclePauseSolid, "interrupted", QUIET, false),
        SessionStatus::Unknown => (icondata::FaCircleQuestionRegular, "unknown", QUIET, false),
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
            icon: icondata::FaPauseSolid,
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
            icon: icondata::FaClockRegular,
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
        ThemePreference::System => icondata::FaCircleHalfStrokeSolid,
        ThemePreference::Light => icondata::FaSunRegular,
        ThemePreference::Dark => icondata::FaMoonSolid,
    }
}

/// The permission mode's icon, a shield of its own for each mode (a
/// phone's bottom bar shows only the icon), and what the mode does.
pub fn permission_mode(mode: PermissionMode) -> (Icon, &'static str) {
    match mode {
        PermissionMode::AutoApprove => (
            icondata::FaShieldHalvedSolid,
            "the classifier approves safe calls and asks you about the rest",
        ),
        PermissionMode::AutoApproveN => (
            icondata::FaShieldSolid,
            "the classifier approves safe calls and rejects the rest without asking",
        ),
        PermissionMode::AutoApproveY => (
            icondata::FaShieldHeartSolid,
            "the classifier approves safe calls and approves the rest without asking",
        ),
        PermissionMode::Ask => (
            icondata::FaUserShieldSolid,
            "every call that needs approval asks you",
        ),
        PermissionMode::Yolo => (
            icondata::FaShieldVirusSolid,
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
        let markup = svg(icondata::FaCheckSolid, "text-xs");
        assert!(markup.contains("aria-hidden=\"true\""));
        // The paths are in the sprite, once, under one ID per icon.
        assert!(!markup.contains(icondata::FaCheckSolid.data));
        assert_eq!(markup, svg(icondata::FaCheckSolid, "text-xs"));
        assert_ne!(
            markup,
            svg(icondata::FaBanSolid, "text-xs"),
            "two icons share a symbol"
        );
    }
}
