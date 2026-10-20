//! Where the page is, independent of the browser: the rail's views, the
//! Settings sections, which view a slash command opens, the commands the
//! session's ⋮ menu offers, and the history entries browser Back closes.

use crate::commands::{COMMANDS, Command, CommandSpec};

/// A full view the rail switches to.
#[derive(Clone, Copy, Debug, Default, Eq, Hash, PartialEq)]
pub enum View {
    /// The sessions and the transcript.
    #[default]
    Chat,
    /// The agent documents and their presets.
    Agents,
    /// `config.toml`, providers, MCP servers, plugins, and this browser's
    /// preferences.
    Settings,
    /// The daemon's machines and its runtimes.
    Machines,
    Memory,
}

impl View {
    /// The rail's order.
    pub const ALL: [Self; 5] = [
        Self::Chat,
        Self::Agents,
        Self::Settings,
        Self::Machines,
        Self::Memory,
    ];

    /// The view's name, as its rail button and title bar say it.
    pub const fn label(self) -> &'static str {
        match self {
            Self::Chat => "Chat",
            Self::Agents => "Agents",
            Self::Settings => "Settings",
            Self::Machines => "Machines & runtimes",
            Self::Memory => "Memory",
        }
    }

    /// The `data-view` value of its rail button and page.
    pub const fn id(self) -> &'static str {
        match self {
            Self::Chat => "chat",
            Self::Agents => "agents",
            Self::Settings => "settings",
            Self::Machines => "machines",
            Self::Memory => "memory",
        }
    }
}

/// A part of the Settings view.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum SettingsSection {
    /// `config.toml`.
    Configuration,
    /// The model providers (`/connect`).
    Providers,
    Mcp,
    Plugins,
    /// What this browser keeps: the diagnostics filter and the token.
    Browser,
}

impl SettingsSection {
    /// The order the section list shows.
    pub const ALL: [Self; 5] = [
        Self::Configuration,
        Self::Providers,
        Self::Mcp,
        Self::Plugins,
        Self::Browser,
    ];

    pub const fn label(self) -> &'static str {
        match self {
            Self::Configuration => "Configuration",
            Self::Providers => "Providers",
            Self::Mcp => "MCP servers",
            Self::Plugins => "Plugins",
            Self::Browser => "This browser",
        }
    }

    /// What the section holds, under its name in the list.
    pub const fn note(self) -> &'static str {
        match self {
            Self::Configuration => "the daemon's config.toml",
            Self::Providers => "connect or disconnect a model provider",
            Self::Mcp => "every runtime's MCP servers",
            Self::Plugins => "plugin status and restarts",
            Self::Browser => "diagnostics shown, and forgetting this daemon",
        }
    }

    /// The `data-section` value of its entry.
    pub const fn id(self) -> &'static str {
        match self {
            Self::Configuration => "configuration",
            Self::Providers => "providers",
            Self::Mcp => "mcp",
            Self::Plugins => "plugins",
            Self::Browser => "browser",
        }
    }
}

/// The view, and the Settings section, a command opens: the daemon-wide
/// commands open views; the session's own open sheets (`None`).
pub const fn command_view(command: Command) -> Option<(View, Option<SettingsSection>)> {
    match command {
        Command::Config => Some((View::Settings, Some(SettingsSection::Configuration))),
        Command::Connect => Some((View::Settings, Some(SettingsSection::Providers))),
        Command::Mcp => Some((View::Settings, Some(SettingsSection::Mcp))),
        Command::Plugins => Some((View::Settings, Some(SettingsSection::Plugins))),
        Command::Memory => Some((View::Memory, None)),
        _ => None,
    }
}

/// What the session's ⋮ menu lists, in its order: the session's own
/// commands that have no other place in the chat view (the bottom bar has
/// the agent, model, mode, and approvals; the composer attaches, sends,
/// and cancels; the sessions sidebar starts and opens sessions).
pub const SESSION_MENU: &[Command] = &[
    Command::Runtime,
    Command::Permissions,
    Command::Goal,
    Command::Background,
    Command::Usage,
    Command::Compact,
    Command::Skills,
    Command::Events,
    Command::Preset,
    Command::AgentPanelShow,
    Command::AgentPanelHide,
    Command::AgentPanelAuto,
];

/// The specs of [`SESSION_MENU`], in its order.
pub fn session_menu() -> Vec<&'static CommandSpec> {
    SESSION_MENU
        .iter()
        .filter_map(|command| COMMANDS.iter().find(|spec| spec.command == *command))
        .collect()
}

/// Something open that browser Back closes, innermost last.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Layer {
    /// The phone's drawer, or the sessions sliding over a tablet's
    /// transcript.
    Drawer,
    /// A phone's Settings section, opened from the section list.
    Section,
    /// A document or memory open in its editor.
    Editor,
}

/// What the page does to the browser's history so that it holds one entry
/// per open layer.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum HistoryStep {
    /// Nothing: the history matches.
    Keep,
    /// Push an entry for each of these, in order.
    Push(Vec<Layer>),
    /// Go back this many entries: their layers closed by other means than
    /// Back. Whatever must be pushed after is reconciled once the browser
    /// arrives.
    Back(usize),
}

/// The step from the layers the history holds entries for (`pushed`) to
/// the layers open now (`open`).
pub fn reconcile(pushed: &[Layer], open: &[Layer]) -> HistoryStep {
    let common = pushed
        .iter()
        .zip(open)
        .take_while(|(pushed, open)| pushed == open)
        .count();
    if common < pushed.len() {
        HistoryStep::Back(pushed.len() - common)
    } else if common < open.len() {
        HistoryStep::Push(open[common..].to_vec())
    } else {
        HistoryStep::Keep
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::parse;

    #[test]
    fn daemon_wide_commands_open_views_and_session_ones_open_sheets() {
        let opens = |text: &str| parse(text).and_then(|(command, _)| command_view(command));
        assert_eq!(
            opens("/config"),
            Some((View::Settings, Some(SettingsSection::Configuration)))
        );
        assert_eq!(
            opens("/connect"),
            Some((View::Settings, Some(SettingsSection::Providers)))
        );
        assert_eq!(
            opens("/mcp"),
            Some((View::Settings, Some(SettingsSection::Mcp)))
        );
        assert_eq!(
            opens("/plugin"),
            Some((View::Settings, Some(SettingsSection::Plugins)))
        );
        assert_eq!(opens("/memories"), Some((View::Memory, None)));
        for text in [
            "/runtime",
            "/goal",
            "/permissions",
            "/usage",
            "/events",
            "/new",
        ] {
            assert_eq!(opens(text), None, "{text}");
        }
    }

    #[test]
    fn the_session_menu_holds_only_commands_without_a_view_or_another_place() {
        let menu = session_menu();
        assert_eq!(menu.len(), SESSION_MENU.len());
        for spec in &menu {
            assert_eq!(command_view(spec.command), None, "/{}", spec.name);
        }
        for elsewhere in [
            Command::New,
            Command::Sessions,
            Command::Agent,
            Command::Model,
            Command::Mode,
            Command::Approvals,
            Command::Attach,
            Command::Cancel,
        ] {
            assert!(!SESSION_MENU.contains(&elsewhere), "{elsewhere:?}");
        }
        let names = menu.iter().map(|spec| spec.name).collect::<Vec<_>>();
        assert_eq!(
            &names[..4],
            ["runtime", "permissions", "goal", "background"]
        );
    }

    #[test]
    fn views_and_sections_have_distinct_ids() {
        let mut views = View::ALL.map(View::id).to_vec();
        views.sort_unstable();
        views.dedup();
        assert_eq!(views.len(), View::ALL.len());
        let mut sections = SettingsSection::ALL.map(SettingsSection::id).to_vec();
        sections.sort_unstable();
        sections.dedup();
        assert_eq!(sections.len(), SettingsSection::ALL.len());
        assert_eq!(View::default(), View::Chat);
    }

    #[test]
    fn the_history_gains_an_entry_per_opened_layer_and_gives_back_closed_ones() {
        use Layer::{Drawer, Editor, Section};
        assert_eq!(reconcile(&[], &[]), HistoryStep::Keep);
        assert_eq!(reconcile(&[], &[Drawer]), HistoryStep::Push(vec![Drawer]));
        assert_eq!(
            reconcile(&[Section], &[Section, Editor]),
            HistoryStep::Push(vec![Editor])
        );
        assert_eq!(
            reconcile(&[Section, Editor], &[Section]),
            HistoryStep::Back(1)
        );
        assert_eq!(reconcile(&[Section, Editor], &[]), HistoryStep::Back(2));
        // Closing the drawer by choosing a view that opens a section: back
        // first, the section's entry once the browser is there.
        assert_eq!(reconcile(&[Drawer], &[Section]), HistoryStep::Back(1));
        assert_eq!(
            reconcile(&[Drawer, Section], &[Drawer, Section]),
            HistoryStep::Keep
        );
    }
}
