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
    /// Make a link that lets someone else use the session.
    Share,
    /// Compact the session's context, with an optional focus.
    Compact,
    /// Set the root session's goal, or pause, resume, or cancel it.
    Goal,
    /// Schedule a prompt into the session: the cron job form, in the
    /// Scheduled prompts view (client protocol 1.15).
    Schedule,
    /// Add a webhook that delivers into the session: the webhook form, in
    /// the Webhooks view (client protocol 1.96.0).
    Webhook,
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
        icon: icondata::LuPencil,
        description: "Change the session's title, or go back to the automatic one",
    },
    MenuEntry {
        command: Command::Compact,
        id: "compact",
        label: "Compact context",
        icon: icondata::LuMinimize2,
        description: "Summarize the history so far, with an optional focus",
    },
    MenuEntry {
        command: Command::Goal,
        id: "goal",
        label: "Goal",
        icon: icondata::LuTarget,
        description: "Set an objective to work toward; pause, resume, or cancel it",
    },
    MenuEntry {
        command: Command::Share,
        id: "share",
        label: "Share conversation",
        icon: icondata::LuShare2,
        description: "Make a link that lets someone else use this session",
    },
    MenuEntry {
        command: Command::Schedule,
        id: "schedule",
        label: "Schedule a prompt",
        icon: icondata::LuClock,
        description: "Send this session a prompt later, or on a schedule",
    },
    MenuEntry {
        command: Command::Webhook,
        id: "webhook",
        label: "Add a webhook",
        icon: icondata::LuWebhook,
        description: "A URL that turns each request it receives into a message here",
    },
    MenuEntry {
        command: Command::Skills,
        id: "skills",
        label: "Skills",
        icon: icondata::LuWandSparkles,
        description: "Search the skills and run one",
    },
    MenuEntry {
        command: Command::Background,
        id: "background",
        label: "Background calls",
        icon: icondata::LuHourglass,
        description: "The calls running in the background; stop one",
    },
    MenuEntry {
        command: Command::Runtime,
        id: "runtime",
        label: "Runtimes",
        icon: icondata::LuServer,
        description: "Attach and detach the session's runtimes",
    },
    MenuEntry {
        command: Command::Permissions,
        id: "permissions",
        label: "Permission rules",
        icon: icondata::LuList,
        description: "The session's own allow, ask, and deny rules",
    },
    MenuEntry {
        command: Command::Usage,
        id: "usage",
        label: "Usage",
        icon: icondata::LuCoins,
        description: "Tokens, cost, and the context size",
    },
    MenuEntry {
        command: Command::Events,
        id: "events",
        label: "Diagnostics",
        icon: icondata::LuListFilter,
        description: "Which diagnostic rows the transcript shows",
    },
    MenuEntry {
        command: Command::Preset,
        id: "preset",
        label: "Agent preset",
        icon: icondata::LuSlidersHorizontal,
        description: "The agents' preset for new sessions",
    },
];

/// Whether the menu shows `command` to a daemon that does or does not keep
/// cron jobs (`has_crons`, client protocol 1.15) and webhooks
/// (`has_webhooks`, client protocol 1.96.0).
pub const fn offered(command: Command, has_crons: bool, has_webhooks: bool) -> bool {
    match command {
        Command::Schedule => has_crons,
        Command::Webhook => has_webhooks,
        _ => true,
    }
}

/// Whether a guest is offered `command`: what acts in the session or only
/// shows it, and not what names it, shares it, or sets it up.
#[must_use]
pub const fn offered_to_guest(command: Command) -> bool {
    !matches!(
        command,
        Command::Rename | Command::Share | Command::Schedule | Command::Webhook | Command::Preset
    )
}

/// The menu entry of `command`.
pub fn entry(command: Command) -> &'static MenuEntry {
    MENU.iter()
        .find(|entry| entry.command == command)
        .expect("every command is in the menu")
}

#[cfg(test)]
mod tests {
    use super::{Command, MENU, entry, offered, offered_to_guest};

    #[test]
    fn a_guest_acts_and_reads_but_does_not_set_the_session_up() {
        for command in [
            Command::Compact,
            Command::Goal,
            Command::Skills,
            Command::Background,
            Command::Runtime,
            Command::Permissions,
            Command::Usage,
            Command::Events,
        ] {
            assert!(offered_to_guest(command), "{command:?}");
        }
        for command in [
            Command::Rename,
            Command::Share,
            Command::Schedule,
            Command::Webhook,
            Command::Preset,
        ] {
            assert!(!offered_to_guest(command), "{command:?}");
        }
    }

    #[test]
    fn only_a_daemon_that_keeps_cron_jobs_or_webhooks_offers_them() {
        assert!(offered(Command::Schedule, true, false));
        assert!(!offered(Command::Schedule, false, true));
        assert!(offered(Command::Webhook, false, true));
        assert!(!offered(Command::Webhook, true, false));
        assert!(offered(Command::Goal, false, false));
    }

    #[test]
    fn the_menu_lists_every_command_once_by_a_readable_name() {
        for command in [
            Command::Rename,
            Command::Share,
            Command::Compact,
            Command::Goal,
            Command::Schedule,
            Command::Webhook,
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
