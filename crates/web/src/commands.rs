//! The session's commands, as the title bar's ⋮ menu lists them. The web
//! composer has no slash commands: text that starts with `/` is a message
//! like any other, and every command is a menu entry with a readable name,
//! an icon, and a line saying what it does. Commands with somewhere else to
//! live are not here: the bottom bar picks the agent, the model, and the
//! permission mode, the composer attaches and stops, the sessions list
//! starts and opens sessions, and the rail opens the daemon-wide views.

use icondata::Icon;

/// A command of the session's ⋮ menu. Each opens a sheet, but Rename
/// edits the title in the title bar.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Command {
    /// Rename the session, or go back to its automatic title.
    Rename,
    /// Compact the session's context, with an optional focus.
    Compact,
    /// Set the root session's goal, or pause, resume, or cancel it.
    Goal,
    /// Search the skills and run one.
    Skills,
    /// The background calls, to stop one.
    Background,
    /// The session's runtimes and the daemon's machines.
    Runtime,
    /// The session's permission overrides.
    Permissions,
    /// The session's usage and cost.
    Usage,
    /// Choose which diagnostic rows the transcript shows.
    Events,
    /// The agent preset for new sessions.
    Preset,
}

/// A menu entry: what it runs, its `data-command` id, its name, its icon,
/// and what it does.
pub struct MenuEntry {
    pub command: Command,
    pub id: &'static str,
    pub label: &'static str,
    pub icon: Icon,
    pub description: &'static str,
}

/// The ⋮ menu, in its order: the session's name, what changes the
/// conversation, then what the session runs with, then what it shows.
pub const MENU: &[MenuEntry] = &[
    MenuEntry {
        command: Command::Rename,
        id: "rename",
        label: "Rename",
        icon: icondata::FaPenSolid,
        description: "Change the session's title, or go back to the automatic one",
    },
    MenuEntry {
        command: Command::Compact,
        id: "compact",
        label: "Compact context",
        icon: icondata::FaCompressSolid,
        description: "Summarize the history so far, with an optional focus",
    },
    MenuEntry {
        command: Command::Goal,
        id: "goal",
        label: "Goal",
        icon: icondata::FaBullseyeSolid,
        description: "Set an objective to work toward; pause, resume, or cancel it",
    },
    MenuEntry {
        command: Command::Skills,
        id: "skills",
        label: "Skills",
        icon: icondata::FaWandMagicSparklesSolid,
        description: "Search the skills and run one",
    },
    MenuEntry {
        command: Command::Background,
        id: "background",
        label: "Background calls",
        icon: icondata::FaHourglassHalfSolid,
        description: "The calls running in the background; stop one",
    },
    MenuEntry {
        command: Command::Runtime,
        id: "runtime",
        label: "Runtimes",
        icon: icondata::FaServerSolid,
        description: "Attach and detach the session's runtimes",
    },
    MenuEntry {
        command: Command::Permissions,
        id: "permissions",
        label: "Permission rules",
        icon: icondata::FaListUlSolid,
        description: "The session's own allow, ask, and deny rules",
    },
    MenuEntry {
        command: Command::Usage,
        id: "usage",
        label: "Usage",
        icon: icondata::FaCoinsSolid,
        description: "Tokens, cost, and the context size",
    },
    MenuEntry {
        command: Command::Events,
        id: "events",
        label: "Diagnostics",
        icon: icondata::FaFilterSolid,
        description: "Which diagnostic rows the transcript shows",
    },
    MenuEntry {
        command: Command::Preset,
        id: "preset",
        label: "Agent preset",
        icon: icondata::FaSlidersSolid,
        description: "The agents' preset for new sessions",
    },
];

/// The menu entry of `command`.
pub fn entry(command: Command) -> &'static MenuEntry {
    MENU.iter()
        .find(|entry| entry.command == command)
        .expect("every command is in the menu")
}

#[cfg(test)]
mod tests {
    use super::{Command, MENU, entry};

    #[test]
    fn the_menu_lists_every_command_once_by_a_readable_name() {
        for command in [
            Command::Rename,
            Command::Compact,
            Command::Goal,
            Command::Skills,
            Command::Background,
            Command::Runtime,
            Command::Permissions,
            Command::Usage,
            Command::Events,
            Command::Preset,
        ] {
            assert_eq!(
                MENU.iter().filter(|entry| entry.command == command).count(),
                1,
                "{command:?}"
            );
            assert_eq!(entry(command).command, command);
        }
        assert_eq!(MENU.len(), 10);
        let mut ids = MENU.iter().map(|entry| entry.id).collect::<Vec<_>>();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), MENU.len(), "ids are distinct");
        for entry in MENU {
            // A name and a sentence, not slash syntax.
            assert!(!entry.label.contains('/'), "{}", entry.label);
            assert!(
                entry.label.starts_with(char::is_uppercase),
                "{}",
                entry.label
            );
            assert!(!entry.description.contains('/'), "{}", entry.description);
            assert!(!entry.description.is_empty());
        }
        assert_eq!(
            MENU.iter()
                .map(|entry| entry.id)
                .take(3)
                .collect::<Vec<_>>(),
            ["rename", "compact", "goal"]
        );
    }
}
