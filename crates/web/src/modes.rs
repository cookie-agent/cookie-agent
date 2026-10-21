//! The permission modes the bottom bar's mode picker lists, with the
//! names and one-line descriptions of the permissions guide
//! (`docs/site/concepts/permissions.md`).

use cookie_agent_protocol::PermissionMode;

/// A mode as the picker shows it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ModeChoice {
    pub mode: PermissionMode,
    pub name: &'static str,
    pub description: &'static str,
}

/// Every mode, in the picker's order: the default first.
pub const MODES: [ModeChoice; 5] = [
    ModeChoice {
        mode: PermissionMode::AutoApprove,
        name: "auto-approve",
        description: "The approval agent decides; what it won't approve comes to you.",
    },
    ModeChoice {
        mode: PermissionMode::AutoApproveN,
        name: "auto-approve-n",
        description: "The approval agent decides; what it would pass to you is rejected.",
    },
    ModeChoice {
        mode: PermissionMode::AutoApproveY,
        name: "auto-approve-y",
        description: "The approval agent decides; what it would pass to you is approved once.",
    },
    ModeChoice {
        mode: PermissionMode::Ask,
        name: "ask",
        description: "Every ask comes to you.",
    },
    ModeChoice {
        mode: PermissionMode::Yolo,
        name: "yolo",
        description: "Every ask is approved at once.",
    },
];

/// What holds whatever the mode, under the picker's list.
pub const FOOTNOTE: &str = "Deny rules and the loop guard hold in every mode.";

/// The picker's entry for `mode`.
pub fn choice(mode: PermissionMode) -> ModeChoice {
    MODES
        .into_iter()
        .find(|choice| choice.mode == mode)
        .expect("every mode is listed")
}

/// `mode`'s name, as the chip and the picker say it.
pub fn mode_name(mode: PermissionMode) -> &'static str {
    choice(mode).name
}

#[cfg(test)]
mod tests {
    use cookie_agent_protocol::PermissionMode;

    use super::{MODES, choice, mode_name};

    #[test]
    fn the_picker_lists_every_mode_once_with_the_default_first() {
        let modes = MODES.map(|choice| choice.mode);
        assert_eq!(
            modes,
            [
                PermissionMode::AutoApprove,
                PermissionMode::AutoApproveN,
                PermissionMode::AutoApproveY,
                PermissionMode::Ask,
                PermissionMode::Yolo,
            ]
        );
        assert_eq!(MODES[0].mode, PermissionMode::default());
        for listed in MODES {
            assert_eq!(choice(listed.mode), listed);
            assert!(listed.description.ends_with('.'), "{}", listed.name);
        }
        assert_eq!(mode_name(PermissionMode::AutoApproveN), "auto-approve-n");
        assert_eq!(mode_name(PermissionMode::Yolo), "yolo");
    }
}
