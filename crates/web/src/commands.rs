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
}

/// A command as the menu and `/` know it.
pub struct CommandSpec {
    pub command: Command,
    pub name: &'static str,
    pub aliases: &'static [&'static str],
    pub description: &'static str,
}

/// Every command, in menu order.
pub const COMMANDS: &[CommandSpec] = &[
    CommandSpec {
        command: Command::New,
        name: "new",
        aliases: &[],
        description: "start a new session",
    },
    CommandSpec {
        command: Command::Sessions,
        name: "sessions",
        aliases: &["resume", "load", "continue"],
        description: "open a session",
    },
    CommandSpec {
        command: Command::Agent,
        name: "agent",
        aliases: &[],
        description: "pick the agent",
    },
    CommandSpec {
        command: Command::Model,
        name: "model",
        aliases: &[],
        description: "pick the model and its variant",
    },
    CommandSpec {
        command: Command::Preset,
        name: "preset",
        aliases: &[],
        description: "pick the agent preset for new sessions",
    },
    CommandSpec {
        command: Command::Approvals,
        name: "approvals",
        aliases: &[],
        description: "answer waiting approvals",
    },
    CommandSpec {
        command: Command::Mode,
        name: "mode",
        aliases: &[],
        description: "cycle the permission mode",
    },
    CommandSpec {
        command: Command::Usage,
        name: "usage",
        aliases: &[],
        description: "show usage and cost",
    },
    CommandSpec {
        command: Command::Cancel,
        name: "cancel",
        aliases: &[],
        description: "cancel the running run",
    },
    CommandSpec {
        command: Command::Events,
        name: "events",
        aliases: &[],
        description: "choose which diagnostics show",
    },
];

/// The command a composer text names, when it is exactly `/<name>` or an
/// alias, surrounding whitespace aside.
pub fn parse(text: &str) -> Option<Command> {
    let name = text.trim().strip_prefix('/')?;
    COMMANDS
        .iter()
        .find(|spec| spec.name == name || spec.aliases.contains(&name))
        .map(|spec| spec.command)
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
        assert_eq!(parse("/new"), Some(Command::New));
        assert_eq!(parse(" /resume "), Some(Command::Sessions));
        assert_eq!(parse("/continue"), Some(Command::Sessions));
        assert_eq!(parse("/usage"), Some(Command::Usage));
        assert_eq!(parse("/events"), Some(Command::Events));
        assert_eq!(parse("new"), None);
        assert_eq!(parse("/nothing"), None);
        assert_eq!(parse("/new session"), None);
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
