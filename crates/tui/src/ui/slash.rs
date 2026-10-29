//! Command palette state, presentation, and command definitions.
//!
//! The palette owns its own search text and never reads or writes the
//! message composer. Commands that need input push a follow-up step (a text
//! prompt or an option list) instead of parsing arguments out of free text.

use ratatui::{
    Frame,
    layout::Rect,
    text::{Line, Span},
    widgets::{ListState, Paragraph},
};

use crate::state::EventLevel;

use super::input::{self, InputState};

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum GoalCommand {
    Objective(String),
    Pause,
    Resume,
    Cancel,
}

/// A command that runs as soon as it is chosen.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum SlashCommand {
    Quit,
    ShowAgentPanel,
    HideAgentPanel,
    AutoAgentPanel,
    /// `/transcript reduced`: fold each block's work into one row.
    TranscriptReduced,
    /// `/transcript full`: show every thinking block and tool call.
    TranscriptFull,
    New,
    Preset,
    Agent,
    Model,
    Connect,
    Mcp,
    Runtime,
    Background,
    Plugins,
    Config,
    /// Restart the daemon, when idle or now.
    Restart,
    /// `/onboard`: the onboarding panel.
    Onboard,
    Memory,
    Cron,
    CronNew,
    Permissions,
    Usage,
    Sessions,
    Project,
    /// The session's ACP agent options.
    Options,
    Cancel,
}

/// What choosing a palette entry does: run immediately, or open the step
/// that collects the command's input.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum PaletteAction {
    Run(SlashCommand),
    Goal,
    Compact,
    Events,
    Skills,
    Attach,
    /// `/rename`: sets the title typed after it, resets it with `--auto`,
    /// or asks for it.
    Rename,
}

impl PaletteAction {
    /// Whether the command reads text typed after its name in the search
    /// (`/rename <title>`).
    pub(crate) fn takes_argument(self) -> bool {
        self == Self::Rename
    }
}

pub(crate) struct CommandSpec {
    pub(crate) name: &'static str,
    /// Extra search terms. They match (and rank) like the name but are
    /// never displayed.
    pub(crate) aliases: &'static [&'static str],
    pub(crate) description: &'static str,
    pub(crate) action: PaletteAction,
    /// Writes to the selected session, so the entry is hidden while that
    /// session is a read-only snapshot owned by another process.
    pub(crate) writes_session: bool,
}

impl CommandSpec {
    pub(crate) fn label(&self) -> String {
        format!("/{} — {}", self.name, self.description)
    }
}

pub(crate) const COMMANDS: &[CommandSpec] = &[
    CommandSpec {
        name: "quit",
        aliases: &["q", "exit"],
        description: "exit the TUI",
        action: PaletteAction::Run(SlashCommand::Quit),
        writes_session: false,
    },
    CommandSpec {
        name: "agent-panel show",
        aliases: &[],
        description: "keep the agent panel shown until a different root session is selected",
        action: PaletteAction::Run(SlashCommand::ShowAgentPanel),
        writes_session: false,
    },
    CommandSpec {
        name: "agent-panel hide",
        aliases: &[],
        description: "keep the agent panel hidden until a different root session is selected",
        action: PaletteAction::Run(SlashCommand::HideAgentPanel),
        writes_session: false,
    },
    CommandSpec {
        name: "agent-panel auto",
        aliases: &[],
        description: "show the agent panel only while delegated agents are live",
        action: PaletteAction::Run(SlashCommand::AutoAgentPanel),
        writes_session: false,
    },
    CommandSpec {
        name: "transcript reduced",
        aliases: &[],
        description: "fold each reply's thinking and tool calls into one Worked for row (saved)",
        action: PaletteAction::Run(SlashCommand::TranscriptReduced),
        writes_session: false,
    },
    CommandSpec {
        name: "transcript full",
        aliases: &[],
        description: "show every thinking block and tool call (saved)",
        action: PaletteAction::Run(SlashCommand::TranscriptFull),
        writes_session: false,
    },
    CommandSpec {
        name: "new",
        aliases: &[],
        description: "start a new root session",
        action: PaletteAction::Run(SlashCommand::New),
        writes_session: false,
    },
    CommandSpec {
        name: "preset",
        aliases: &[],
        description: "select the preset for the next root run and future new sessions",
        action: PaletteAction::Run(SlashCommand::Preset),
        writes_session: false,
    },
    CommandSpec {
        name: "options",
        aliases: &[],
        description: "change the session's ACP agent options (model, mode, effort)",
        action: PaletteAction::Run(SlashCommand::Options),
        writes_session: true,
    },
    CommandSpec {
        name: "agent",
        aliases: &[],
        description: "choose the agent for the next run",
        action: PaletteAction::Run(SlashCommand::Agent),
        writes_session: false,
    },
    CommandSpec {
        name: "model",
        aliases: &[],
        description: "choose the model, then its variant, for the next run",
        action: PaletteAction::Run(SlashCommand::Model),
        writes_session: false,
    },
    CommandSpec {
        name: "connect",
        aliases: &[],
        description: "securely connect a model provider",
        action: PaletteAction::Run(SlashCommand::Connect),
        writes_session: false,
    },
    CommandSpec {
        name: "mcp",
        aliases: &[],
        description: "manage MCP servers",
        action: PaletteAction::Run(SlashCommand::Mcp),
        writes_session: false,
    },
    CommandSpec {
        name: "runtime",
        aliases: &["runtimes"],
        description: "attach and detach the session's runtimes",
        action: PaletteAction::Run(SlashCommand::Runtime),
        writes_session: true,
    },
    CommandSpec {
        name: "plugins",
        aliases: &["plugin"],
        description: "show plugin status and restart a plugin",
        action: PaletteAction::Run(SlashCommand::Plugins),
        writes_session: false,
    },
    CommandSpec {
        name: "config",
        aliases: &[],
        description: "edit the daemon's config.toml and agent documents",
        action: PaletteAction::Run(SlashCommand::Config),
        writes_session: false,
    },
    CommandSpec {
        name: "restart",
        aliases: &[],
        description: "restart the daemon, once idle or now, to apply its configuration or a new build",
        action: PaletteAction::Run(SlashCommand::Restart),
        writes_session: false,
    },
    CommandSpec {
        name: "onboard",
        aliases: &[],
        description: "set up Cookie: a provider, the default and small models, the starter kit",
        action: PaletteAction::Run(SlashCommand::Onboard),
        writes_session: false,
    },
    CommandSpec {
        name: "memory",
        aliases: &["memories"],
        description: "browse, edit, create, and delete the daemon's memories",
        action: PaletteAction::Run(SlashCommand::Memory),
        writes_session: false,
    },
    CommandSpec {
        name: "cron",
        aliases: &["crons"],
        description: "list, create, edit, run, and delete the daemon's scheduled prompts",
        action: PaletteAction::Run(SlashCommand::Cron),
        writes_session: false,
    },
    CommandSpec {
        name: "cron new",
        aliases: &[],
        description: "schedule a prompt in this session",
        action: PaletteAction::Run(SlashCommand::CronNew),
        writes_session: true,
    },
    CommandSpec {
        name: "background",
        aliases: &["bg"],
        description: "list and stop the session's background calls",
        action: PaletteAction::Run(SlashCommand::Background),
        writes_session: false,
    },
    CommandSpec {
        name: "permissions",
        aliases: &["perms"],
        description: "edit session permission overrides",
        action: PaletteAction::Run(SlashCommand::Permissions),
        writes_session: true,
    },
    CommandSpec {
        name: "skills",
        aliases: &[],
        description: "search skills and run one",
        action: PaletteAction::Skills,
        writes_session: true,
    },
    CommandSpec {
        name: "sessions",
        aliases: &["resume", "load", "continue"],
        description: "choose a session",
        action: PaletteAction::Run(SlashCommand::Sessions),
        writes_session: false,
    },
    CommandSpec {
        name: "project",
        aliases: &["projects"],
        description: "choose the project of this session, or of the new one",
        action: PaletteAction::Run(SlashCommand::Project),
        writes_session: false,
    },
    CommandSpec {
        name: "rename",
        aliases: &["title"],
        description: "rename this session: /rename <title>, or --auto for the automatic title",
        action: PaletteAction::Rename,
        writes_session: true,
    },
    CommandSpec {
        name: "usage",
        aliases: &[],
        description: "show session and global token usage",
        action: PaletteAction::Run(SlashCommand::Usage),
        writes_session: false,
    },
    CommandSpec {
        name: "cancel",
        aliases: &[],
        description: "cancel the active run",
        action: PaletteAction::Run(SlashCommand::Cancel),
        writes_session: true,
    },
    CommandSpec {
        name: "attach",
        aliases: &["file", "image"],
        description: "attach a file to the next message (asks for its path)",
        action: PaletteAction::Attach,
        writes_session: true,
    },
    CommandSpec {
        name: "goal",
        aliases: &[],
        description: "set a root-session goal (asks for the objective)",
        action: PaletteAction::Goal,
        writes_session: true,
    },
    CommandSpec {
        name: "compact",
        aliases: &[],
        description: "compact context, optionally emphasizing a focus",
        action: PaletteAction::Compact,
        writes_session: true,
    },
    CommandSpec {
        name: "events",
        aliases: &[],
        description: "choose the diagnostic level filter for this view",
        action: PaletteAction::Events,
        writes_session: false,
    },
];

pub(crate) const EVENT_LEVELS: [EventLevel; 4] = [
    EventLevel::Debug,
    EventLevel::Info,
    EventLevel::Warning,
    EventLevel::Error,
];

/// How well `query` matches any of `candidates`: exact beats prefix beats
/// substring. `None` means no match. An empty query matches everything.
pub(crate) fn match_rank<'a>(
    candidates: impl IntoIterator<Item = &'a str>,
    query: &str,
) -> Option<u8> {
    if query.is_empty() {
        return Some(0);
    }
    candidates
        .into_iter()
        .filter_map(|candidate| {
            if candidate == query {
                Some(0)
            } else if candidate.starts_with(query) {
                Some(1)
            } else if candidate.contains(query) {
                Some(2)
            } else {
                None
            }
        })
        .min()
}

/// Commands matching `query`, best match first and registry order within a
/// rank.
pub(crate) fn entries(query: &str) -> Vec<&'static CommandSpec> {
    let raw = query;
    let query = normalized_query(query);
    let mut ranked = COMMANDS
        .iter()
        .filter_map(|spec| {
            if command_argument(spec, raw).is_some() {
                return Some((0, spec));
            }
            match_rank(
                std::iter::once(spec.name).chain(spec.aliases.iter().copied()),
                &query,
            )
            .map(|rank| (rank, spec))
        })
        .collect::<Vec<_>>();
    ranked.sort_by_key(|(rank, _)| *rank);
    ranked.into_iter().map(|(_, spec)| spec).collect()
}

/// The text typed after `spec`'s name (or an alias) and a space in the
/// palette's search, as typed, for a command that takes one.
pub(crate) fn command_argument<'a>(spec: &CommandSpec, query: &'a str) -> Option<&'a str> {
    if !spec.action.takes_argument() {
        return None;
    }
    let query = query.trim_start();
    let (head, rest) = query.split_once(char::is_whitespace)?;
    std::iter::once(spec.name)
        .chain(spec.aliases.iter().copied())
        .any(|name| head.eq_ignore_ascii_case(name))
        .then_some(rest)
}

pub(crate) fn normalized_query(query: &str) -> String {
    query.trim().to_lowercase()
}

/// What a text step's input is for.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum TextTarget {
    GoalObjective,
    CompactFocus,
    SkillArguments {
        name: String,
        hint: Option<String>,
    },
    /// The path of a file to attach; `accepts` names what the model takes.
    AttachPath {
        accepts: String,
    },
    /// The title of `session_id`, prefilled with its current one.
    SessionTitle {
        session_id: cookie_agent_protocol::SessionId,
    },
    /// The input of an ACP agent's command `name`, sent as `/<name>
    /// <input>`.
    AgentCommand {
        name: String,
        hint: Option<String>,
    },
}

pub(crate) enum PaletteStep {
    Text {
        target: TextTarget,
        input: InputState,
    },
    EventLevel {
        list: ListState,
    },
    Skills {
        search: InputState,
        list: ListState,
    },
}

/// The open palette: the command search plus a stack of follow-up steps.
/// Esc pops one step; with no steps left it closes the palette.
pub(crate) struct CommandPalette {
    pub(crate) search: InputState,
    pub(crate) list: ListState,
    pub(crate) steps: Vec<PaletteStep>,
}

impl CommandPalette {
    pub(crate) fn new() -> Self {
        Self {
            search: InputState::default(),
            list: ListState::default().with_selected(Some(0)),
            steps: Vec::new(),
        }
    }

    /// The text field keys and pastes edit in the current step, if any.
    pub(crate) fn active_input(&mut self) -> Option<&mut InputState> {
        match self.steps.last_mut() {
            None => Some(&mut self.search),
            Some(PaletteStep::Text { input, .. }) => Some(input),
            Some(PaletteStep::Skills { search, .. }) => Some(search),
            Some(PaletteStep::EventLevel { .. }) => None,
        }
    }

    /// The list the current step selects from, if any.
    pub(crate) fn active_list(&mut self) -> Option<&mut ListState> {
        match self.steps.last_mut() {
            None => Some(&mut self.list),
            Some(PaletteStep::EventLevel { list } | PaletteStep::Skills { list, .. }) => Some(list),
            Some(PaletteStep::Text { .. }) => None,
        }
    }
}

impl Default for CommandPalette {
    fn default() -> Self {
        Self::new()
    }
}

pub(crate) fn move_selection(state: &mut ListState, len: usize, up: bool) {
    if len == 0 {
        state.select(Some(0));
        return;
    }
    let selected = state.selected().unwrap_or(0);
    state.select(Some(if up {
        selected.saturating_sub(1)
    } else {
        (selected + 1).min(len - 1)
    }));
}

/// The chrome of a list step: panel titles, key hints, and what to say when
/// nothing matches.
pub(crate) struct ListChrome<'a> {
    pub(crate) title: &'a str,
    pub(crate) hint: &'a str,
    pub(crate) empty_message: &'a str,
    /// A one-line description of the highlighted row, under the list.
    pub(crate) detail: Option<String>,
}

/// A list step, optionally headed by a search field. Returns the visible
/// row hit regions.
pub(crate) fn render_list(
    frame: &mut Frame,
    area: Rect,
    chrome: ListChrome<'_>,
    search: Option<(&mut InputState, &str)>,
    entries: Vec<String>,
    state: &mut ListState,
    theme: &crate::theme::Theme,
) -> Vec<(Rect, usize)> {
    let entries = entries.into_iter().map(|label| (label, None)).collect();
    render_badged_list(frame, area, chrome, search, entries, state, theme)
}

/// [`render_list`] whose rows may start with a badge, such as the agent's
/// commands' `agent`.
pub(crate) fn render_badged_list(
    frame: &mut Frame,
    area: Rect,
    chrome: ListChrome<'_>,
    search: Option<(&mut InputState, &str)>,
    entries: Vec<(String, Option<&'static str>)>,
    state: &mut ListState,
    theme: &crate::theme::Theme,
) -> Vec<(Rect, usize)> {
    super::app::paint_panel(frame, area, theme);
    let search_height = if search.is_some() {
        3.min(area.height)
    } else {
        0
    };
    if let Some((input, placeholder)) = search {
        input::render(
            frame,
            Rect::new(area.x, area.y, area.width, search_height),
            input,
            true,
            "Search",
            Some(placeholder),
            theme,
        );
    }
    let detail_height = u16::from(chrome.detail.is_some() && area.height > search_height + 3);
    let list_area = Rect::new(
        area.x,
        area.y.saturating_add(search_height),
        area.width,
        area.height
            .saturating_sub(search_height)
            .saturating_sub(detail_height),
    );
    if detail_height > 0
        && let Some(detail) = chrome.detail
    {
        let width = usize::from(area.width.saturating_sub(2));
        frame.render_widget(
            Paragraph::new(Span::styled(
                super::app::truncate_with_ellipsis(&detail, width),
                theme.muted(),
            )),
            Rect::new(
                area.x.saturating_add(1),
                list_area.y.saturating_add(list_area.height),
                area.width.saturating_sub(2),
                1,
            ),
        );
    }
    // Labels end in an ellipsis rather than clip, after the selection
    // marker's two columns and the badge.
    let available = usize::from(list_area.width.saturating_sub(4));
    let lines = entries
        .into_iter()
        .map(|(label, badge)| match badge {
            Some(badge) => {
                let badge = format!(" {badge} ");
                let room = available.saturating_sub(badge.chars().count() + 1);
                Line::from(vec![
                    Span::styled(
                        badge,
                        theme
                            .internal()
                            .add_modifier(ratatui::style::Modifier::REVERSED),
                    ),
                    Span::raw(" "),
                    Span::raw(super::app::truncate_with_ellipsis(&label, room)),
                ])
            }
            None => Line::from(super::app::truncate_with_ellipsis(&label, available)),
        })
        .collect();
    super::pickers::render_lines(
        frame,
        super::pickers::PickerChrome {
            title: chrome.title,
            empty_message: Some(chrome.empty_message),
            hint: Some(chrome.hint),
        },
        lines,
        list_area,
        state,
        theme,
        theme.selected(),
    )
}

/// A text step: one input box under a title, with a key hint below it.
/// Only the rows the box needs are painted, so the step floats compactly at
/// the top of `area`. Returns the painted rectangle.
pub(crate) fn render_text(
    frame: &mut Frame,
    area: Rect,
    title: &str,
    placeholder: &str,
    hint: &str,
    input: &mut InputState,
    theme: &crate::theme::Theme,
) -> Rect {
    let rows = u16::try_from(input.composer_rows(area.width.saturating_sub(2)))
        .unwrap_or(u16::MAX)
        .clamp(1, input::MAX_TEXT_ROWS);
    let box_height = rows.saturating_add(2).min(area.height);
    let painted = Rect::new(
        area.x,
        area.y,
        area.width,
        box_height.saturating_add(1).min(area.height),
    );
    super::app::paint_panel(frame, painted, theme);
    input::render(
        frame,
        Rect::new(area.x, area.y, area.width, box_height),
        input,
        true,
        title.to_owned(),
        Some(placeholder),
        theme,
    );
    if painted.height > box_height {
        frame.render_widget(
            Paragraph::new(
                Line::from(Span::styled(hint.to_owned(), theme.internal())).right_aligned(),
            ),
            Rect::new(
                area.x.saturating_add(1),
                area.y.saturating_add(box_height),
                area.width.saturating_sub(2),
                1,
            ),
        );
    }
    painted
}

#[cfg(test)]
mod tests {
    use ratatui::{Terminal, backend::TestBackend, widgets::ListState};

    use super::{COMMANDS, PaletteAction, SlashCommand, command_argument, entries, match_rank};

    fn names(query: &str) -> Vec<&'static str> {
        entries(query).into_iter().map(|spec| spec.name).collect()
    }

    #[test]
    fn search_ranks_exact_then_prefix_then_substring() {
        assert_eq!(names("").len(), COMMANDS.len());
        // The exact name first; `/cron new` only contains it.
        assert_eq!(names("new"), ["new", "cron new"]);
        assert_eq!(names("cron"), ["cron", "cron new"]);
        assert_eq!(names("model"), ["model"]);
        // The exact name outranks the panel modes it merely prefixes.
        assert_eq!(
            names("agent"),
            [
                "agent",
                "agent-panel show",
                "agent-panel hide",
                "agent-panel auto"
            ]
        );
        assert_eq!(names("agent-panel h"), ["agent-panel hide"]);
        // "sessions" contains "s" everywhere; the exact alias still wins.
        assert_eq!(names("q").first(), Some(&"quit"));
        assert_eq!(names("perms"), ["permissions"]);
        assert_eq!(names("exit"), ["quit"]);
        for alias in ["resume", "load", "continue"] {
            assert_eq!(names(alias), ["sessions"], "{alias}");
        }
        // Aliases search but never show.
        let sessions = entries("resume")[0];
        assert_eq!(sessions.label(), "/sessions — choose a session");
        assert_eq!(names("hide"), ["agent-panel hide"]);
        assert_eq!(names("  GOAL "), ["goal"]);
        let ranked = names("s");
        assert_eq!(ranked.first(), Some(&"skills"), "{ranked:?}");
        assert!(names("definitely-not-a-command").is_empty());
        assert_eq!(match_rank(["compact"], "pact"), Some(2));
        assert_eq!(match_rank(["compact"], "comp"), Some(1));
        assert_eq!(match_rank(["compact"], "compact"), Some(0));
        assert_eq!(match_rank(["compact"], "x"), None);
    }

    #[test]
    fn retired_commands_are_not_in_the_registry() {
        for retired in [
            "approve", "block", "scroll", "stdin", "eof", "tree", "watch", "message", "help",
        ] {
            assert!(
                COMMANDS
                    .iter()
                    .all(|spec| spec.name != retired && !spec.aliases.contains(&retired)),
                "{retired}"
            );
        }
    }

    #[test]
    fn argument_commands_open_steps_instead_of_running() {
        let action = |name: &str| {
            COMMANDS
                .iter()
                .find(|spec| spec.name == name)
                .map(|spec| spec.action)
        };
        assert_eq!(action("goal"), Some(PaletteAction::Goal));
        assert_eq!(action("compact"), Some(PaletteAction::Compact));
        assert_eq!(action("events"), Some(PaletteAction::Events));
        assert_eq!(action("skills"), Some(PaletteAction::Skills));
        assert_eq!(action("quit"), Some(PaletteAction::Run(SlashCommand::Quit)));
        assert_eq!(action("rename"), Some(PaletteAction::Rename));
    }

    #[test]
    fn rename_keeps_the_text_typed_after_it() {
        let rename = COMMANDS
            .iter()
            .find(|spec| spec.name == "rename")
            .expect("rename");
        assert_eq!(names("rename Fix The Parser"), ["rename"]);
        assert_eq!(names("Title --auto"), ["rename"]);
        assert_eq!(
            command_argument(rename, "rename Fix The Parser"),
            Some("Fix The Parser")
        );
        assert_eq!(command_argument(rename, "  title --auto"), Some("--auto"));
        assert_eq!(command_argument(rename, "rename"), None);
        assert_eq!(command_argument(rename, "renamed x"), None);
        // Other commands never read an argument.
        let goal = COMMANDS.iter().find(|spec| spec.name == "goal").unwrap();
        assert_eq!(command_argument(goal, "goal ship it"), None);
        assert!(names("goal ship it").is_empty());
    }

    #[test]
    fn list_rows_ellipsize_and_the_footer_explains_the_keys() {
        let theme = crate::theme::Theme::default();
        let mut terminal = Terminal::new(TestBackend::new(34, 12)).expect("terminal");
        let mut state = ListState::default().with_selected(Some(0));
        let mut search = crate::ui::input::InputState::default();
        terminal
            .draw(|frame| {
                super::render_list(
                    frame,
                    frame.area(),
                    super::ListChrome {
                        title: "Commands",
                        hint: "enter: choose · esc: close",
                        empty_message: "No matching commands",
                        detail: None,
                    },
                    Some((&mut search, "Filter commands…")),
                    vec![
                        "/goal — set a root-session goal (asks for the objective)".to_owned(),
                        "/quit — exit the TUI".to_owned(),
                    ],
                    &mut state,
                    &theme,
                );
            })
            .expect("render palette");
        let buffer = terminal.backend().buffer();
        let text = (0..12)
            .flat_map(|y| (0..34).map(move |x| buffer[(x, y)].symbol().to_owned()))
            .collect::<String>();
        assert!(text.contains('…'), "{text}");
        assert!(!text.contains("asks for the objective"), "{text}");
        assert!(text.contains("Search"), "{text}");
        // No row spills over the right border.
        assert_eq!(buffer[(33, 4)].symbol(), "│");
        assert!(text.contains("enter: choose"), "{text}");
    }
}
