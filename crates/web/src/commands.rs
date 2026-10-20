//! The slash commands the page has, with the TUI's names and aliases. The
//! command menu lists every one, as phones lack a convenient `/`.

/// A command the page runs.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Command {
    /// Start a new session.
    New,
    /// Pick a session to open.
    Sessions,
    Agent,
    Model,
    Preset,
    /// The session's usage and cost.
    Usage,
    /// Cancel the running run.
    Cancel,
    /// Choose which diagnostic rows the transcript shows.
    Events,
    /// The approvals waiting for an answer.
    Approvals,
    /// Cycle the session tree's permission mode.
    Mode,
    /// Set the root session's goal (`/goal <objective>`), or pause,
    /// resume, or cancel it.
    Goal,
    /// Compact the session's context, with an optional focus.
    Compact,
    /// The background calls, to stop one.
    Background,
    /// Search the skills and run one.
    Skills,
    /// The session's permission overrides.
    Permissions,
    /// Attach a file to the next message.
    Attach,
    AgentPanelShow,
    AgentPanelHide,
    AgentPanelAuto,
}

/// A command as the menu and `/` know it.
pub struct CommandSpec {
    pub command: Command,
    pub name: &'static str,
    pub aliases: &'static [&'static str],
    pub description: &'static str,
    /// What follows the name is the command's argument, as for `/goal`.
    pub takes_argument: bool,
}

/// Every command, in menu order.
pub const COMMANDS: &[CommandSpec] = &[
    CommandSpec {
        command: Command::New,
        name: "new",
        aliases: &[],
        description: "start a new session",
        takes_argument: false,
    },
    CommandSpec {
        command: Command::Sessions,
        name: "sessions",
        aliases: &["resume", "load", "continue"],
        description: "open a session",
        takes_argument: false,
    },
    CommandSpec {
        command: Command::Agent,
        name: "agent",
        aliases: &[],
        description: "pick the agent",
        takes_argument: false,
    },
    CommandSpec {
        command: Command::Model,
        name: "model",
        aliases: &[],
        description: "pick the model and its variant",
        takes_argument: false,
    },
    CommandSpec {
        command: Command::Preset,
        name: "preset",
        aliases: &[],
        description: "pick the agent preset for new sessions",
        takes_argument: false,
    },
    CommandSpec {
        command: Command::Approvals,
        name: "approvals",
        aliases: &[],
        description: "answer waiting approvals",
        takes_argument: false,
    },
    CommandSpec {
        command: Command::Mode,
        name: "mode",
        aliases: &[],
        description: "cycle the permission mode",
        takes_argument: false,
    },
    CommandSpec {
        command: Command::Usage,
        name: "usage",
        aliases: &[],
        description: "show usage and cost",
        takes_argument: false,
    },
    CommandSpec {
        command: Command::Cancel,
        name: "cancel",
        aliases: &[],
        description: "cancel the running run",
        takes_argument: false,
    },
    CommandSpec {
        command: Command::Events,
        name: "events",
        aliases: &[],
        description: "choose which diagnostics show",
        takes_argument: false,
    },
    CommandSpec {
        command: Command::Goal,
        name: "goal",
        aliases: &[],
        description: "set the goal, or pause, resume, or cancel it",
        takes_argument: true,
    },
    CommandSpec {
        command: Command::Compact,
        name: "compact",
        aliases: &[],
        description: "compact the context, with an optional focus",
        takes_argument: true,
    },
    CommandSpec {
        command: Command::Background,
        name: "background",
        aliases: &["bg"],
        description: "the calls running in the background",
        takes_argument: false,
    },
    CommandSpec {
        command: Command::Skills,
        name: "skills",
        aliases: &[],
        description: "search and run a skill",
        takes_argument: true,
    },
    CommandSpec {
        command: Command::Permissions,
        name: "permissions",
        aliases: &["perms"],
        description: "the session's permission overrides",
        takes_argument: false,
    },
    CommandSpec {
        command: Command::Attach,
        name: "attach",
        aliases: &["file", "image"],
        description: "attach a file to the next message",
        takes_argument: false,
    },
    CommandSpec {
        command: Command::AgentPanelShow,
        name: "agent-panel show",
        aliases: &[],
        description: "always show the subagent tree",
        takes_argument: false,
    },
    CommandSpec {
        command: Command::AgentPanelHide,
        name: "agent-panel hide",
        aliases: &[],
        description: "never show the subagent tree",
        takes_argument: false,
    },
    CommandSpec {
        command: Command::AgentPanelAuto,
        name: "agent-panel auto",
        aliases: &[],
        description: "show the subagent tree while agents run",
        takes_argument: false,
    },
];

/// The command a composer text names and its argument: `/<name>` or an
/// alias, followed by an argument only for a command that takes one.
pub fn parse(text: &str) -> Option<(Command, String)> {
    let text = text.trim().strip_prefix('/')?;
    COMMANDS.iter().find_map(|spec| {
        std::iter::once(spec.name)
            .chain(spec.aliases.iter().copied())
            .find_map(|name| {
                let rest = text.strip_prefix(name)?;
                let argument = if rest.is_empty() {
                    ""
                } else {
                    rest.strip_prefix(char::is_whitespace)?.trim()
                };
                (argument.is_empty() || spec.takes_argument)
                    .then(|| (spec.command, argument.to_owned()))
            })
    })
}

/// The commands whose names or aliases start with what follows `/` in
/// `text`, for the menu while the user types one.
pub fn completions(text: &str) -> Vec<&'static CommandSpec> {
    let Some(prefix) = text.trim_start().strip_prefix('/') else {
        return Vec::new();
    };
    COMMANDS
        .iter()
        .filter(|spec| {
            spec.name.starts_with(prefix)
                || spec.aliases.iter().any(|alias| alias.starts_with(prefix))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::{Command, completions, parse};

    #[test]
    fn names_and_aliases_parse_as_in_the_tui() {
        let command = |text: &str| parse(text).map(|(command, _)| command);
        assert_eq!(command("/new"), Some(Command::New));
        assert_eq!(command(" /resume "), Some(Command::Sessions));
        assert_eq!(command("/continue"), Some(Command::Sessions));
        assert_eq!(command("/usage"), Some(Command::Usage));
        assert_eq!(command("/events"), Some(Command::Events));
        assert_eq!(command("/bg"), Some(Command::Background));
        assert_eq!(command("/perms"), Some(Command::Permissions));
        assert_eq!(command("/image"), Some(Command::Attach));
        assert_eq!(command("/agent-panel hide"), Some(Command::AgentPanelHide));
        assert_eq!(command("new"), None);
        assert_eq!(command("/nothing"), None);
        assert_eq!(command("/new session"), None);
        assert_eq!(command("/newer"), None);
        assert_eq!(
            parse("/goal ship the release "),
            Some((Command::Goal, "ship the release".into()))
        );
        assert_eq!(parse("/goal"), Some((Command::Goal, String::new())));
        assert_eq!(
            parse("/compact keep the API notes"),
            Some((Command::Compact, "keep the API notes".into()))
        );
    }

    #[test]
    fn completions_follow_the_typed_prefix() {
        let names = completions("/m")
            .into_iter()
            .map(|spec| spec.name)
            .collect::<Vec<_>>();
        assert_eq!(names, ["model", "mode"]);
        assert!(completions("hello").is_empty());
        assert_eq!(completions("/").len(), super::COMMANDS.len());
    }
}
