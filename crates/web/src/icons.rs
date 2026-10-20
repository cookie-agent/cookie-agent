//! The page's icons: Font Awesome Free 6, whose artwork is CC BY 4.0, from
//! the `icondata` crate. Components show them through `leptos_icons`; the
//! transcript, which renders to HTML text, inlines the same SVG. Each icon
//! is a static the linker keeps only when the page names it.
//!
//! Icons inherit the text colour (`currentColor`) and are hidden from
//! assistive technology: the button or text beside them carries the name.

use cookie_agent_client_state::ToolStatus;
use cookie_agent_protocol::{PermissionMode, SessionStatus, ToolHint};
pub use icondata::Icon;
use leptos::prelude::*;

use crate::theme::ThemePreference;

/// Font Awesome's credit line, which its licence asks the page to show.
pub const CREDIT: &str = "Icons: Font Awesome Free by Fonticons, Inc., CC BY 4.0";
/// Where the credit links: the licence.
pub const CREDIT_URL: &str = "https://fontawesome.com/license/free";

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
/// `<svg>`.
pub fn svg(icon: Icon, class: &str) -> String {
    format!(
        "<svg class=\"inline-block shrink-0 {class}\" aria-hidden=\"true\" width=\"1em\" height=\"1em\" viewBox=\"{}\" fill=\"currentColor\">{}</svg>",
        icon.view_box.unwrap_or("0 0 512 512"),
        icon.data
    )
}

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
    use cookie_agent_protocol::{SessionStatus, ToolHint};

    use super::{hint_icon, permission_mode, session_status, svg, tool_status};

    #[test]
    fn every_permission_mode_has_its_own_icon() {
        use cookie_agent_protocol::PermissionMode;
        let mut icons = [
            PermissionMode::AutoApprove,
            PermissionMode::AutoApproveN,
            PermissionMode::AutoApproveY,
            PermissionMode::Ask,
            PermissionMode::Yolo,
        ]
        .map(|mode| permission_mode(mode).0.data)
        .to_vec();
        icons.sort_unstable();
        icons.dedup();
        assert_eq!(icons.len(), 5);
    }

    #[test]
    fn every_documented_hint_has_its_own_icon_and_others_are_generic() {
        let generic = hint_icon(Some(ToolHint::GENERIC));
        assert_eq!(hint_icon(None), generic);
        assert_eq!(hint_icon(Some("teleport")), generic);
        assert_eq!(hint_icon(Some("shell")), icondata::FaTerminalSolid);
        assert_eq!(
            hint_icon(Some(ToolHint::FILE_WRITE)),
            hint_icon(Some(ToolHint::FILE_EDIT))
        );
        let mut distinct = ToolHint::DOCUMENTED
            .iter()
            .filter(|hint| **hint != ToolHint::FILE_EDIT)
            .map(|hint| hint_icon(Some(hint)).data)
            .collect::<Vec<_>>();
        let documented = distinct.len();
        distinct.sort_unstable();
        distinct.dedup();
        assert_eq!(distinct.len(), documented, "two hints share an icon");
    }

    #[test]
    fn statuses_name_themselves_and_only_running_ones_spin() {
        for status in [
            ToolStatus::Running,
            ToolStatus::Completed,
            ToolStatus::Failed,
            ToolStatus::Cancelled,
            ToolStatus::Interrupted,
        ] {
            let mark = tool_status(&status, false);
            assert_eq!(mark.spins, status == ToolStatus::Running, "{status:?}");
            assert!(!mark.label.is_empty());
        }
        let backgrounded = tool_status(&ToolStatus::Running, true);
        assert!(!backgrounded.spins);
        assert_eq!(backgrounded.label, "in the background");
        assert_eq!(tool_status(&ToolStatus::Failed, false).label, "failed");

        assert!(session_status(SessionStatus::Running).spins);
        assert_eq!(session_status(SessionStatus::Completed).label, "completed");
        assert_eq!(
            session_status(SessionStatus::Failed).icon,
            tool_status(&ToolStatus::Failed, false).icon
        );
        assert_eq!(session_status(SessionStatus::Unknown).label, "unknown");
    }

    #[test]
    fn inline_svg_is_hidden_and_takes_the_text_colour() {
        let markup = svg(icondata::FaCheckSolid, "text-xs");
        assert!(markup.starts_with("<svg class=\"inline-block shrink-0 text-xs\""));
        assert!(markup.contains("aria-hidden=\"true\""));
        assert!(markup.contains("fill=\"currentColor\""));
        assert!(markup.contains(icondata::FaCheckSolid.data));
    }
}
