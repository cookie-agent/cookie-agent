//! Slash-command palette presentation and command definitions.

use cookie_agent_protocol::ApprovalUserDecision;
use ratatui::{
    Frame,
    layout::Rect,
    widgets::{Block, Borders, List, ListItem, ListState, Paragraph},
};

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum SlashCommand {
    Quit,
    New,
    Connect,
    Mcp,
    Permissions,
    Skills,
    Skill { name: String, args: String },
    Usage,
    Sessions,
    Cancel,
    Compact(Option<String>),
    Approve(ApprovalUserDecision),
    Events(crate::state::EventLevel),
    Help,
}

pub(crate) struct CommandSpec {
    pub(crate) name: &'static str,
    pub(crate) aliases: &'static [&'static str],
    pub(crate) usage: &'static str,
    pub(crate) description: &'static str,
    pub(crate) requires_arguments: bool,
}

pub(crate) const COMMANDS: &[CommandSpec] = &[
    CommandSpec {
        name: "quit",
        aliases: &["q"],
        usage: "/quit",
        description: "exit the TUI",
        requires_arguments: false,
    },
    CommandSpec {
        name: "new",
        aliases: &[],
        usage: "/new",
        description: "choose the next run agent",
        requires_arguments: false,
    },
    CommandSpec {
        name: "connect",
        aliases: &[],
        usage: "/connect",
        description: "securely connect a model provider",
        requires_arguments: false,
    },
    CommandSpec {
        name: "mcp",
        aliases: &[],
        usage: "/mcp",
        description: "manage MCP servers",
        requires_arguments: false,
    },
    CommandSpec {
        name: "permissions",
        aliases: &["perms"],
        usage: "/permissions",
        description: "edit session permission overrides",
        requires_arguments: false,
    },
    CommandSpec {
        name: "skills",
        aliases: &[],
        usage: "/skills",
        description: "show discovered skills",
        requires_arguments: false,
    },
    CommandSpec {
        name: "sessions",
        aliases: &[],
        usage: "/sessions",
        description: "choose a session",
        requires_arguments: false,
    },
    CommandSpec {
        name: "usage",
        aliases: &[],
        usage: "/usage",
        description: "show session and global token usage",
        requires_arguments: false,
    },
    CommandSpec {
        name: "cancel",
        aliases: &[],
        usage: "/cancel",
        description: "cancel the active run",
        requires_arguments: false,
    },
    CommandSpec {
        name: "compact",
        aliases: &[],
        usage: "/compact [focus]",
        description: "compact context, optionally emphasizing a focus",
        requires_arguments: false,
    },
    CommandSpec {
        name: "approve",
        aliases: &[],
        usage: "/approve once|all|reject|cancel",
        description: "answer an approval",
        requires_arguments: true,
    },
    CommandSpec {
        name: "events",
        aliases: &[],
        usage: "/events debug|info|warning|error",
        description: "set the diagnostic level filter for this view",
        requires_arguments: true,
    },
    CommandSpec {
        name: "help",
        aliases: &[],
        usage: "/help",
        description: "show command help",
        requires_arguments: false,
    },
];

#[derive(Debug, Eq, PartialEq)]
pub(crate) enum Submission {
    Prompt(String),
    Command(SlashCommand),
}

pub(crate) fn entries(input: &str) -> Vec<&'static CommandSpec> {
    let query = input
        .strip_prefix('/')
        .unwrap_or_default()
        .split_whitespace()
        .next()
        .unwrap_or_default()
        .to_ascii_lowercase();
    COMMANDS
        .iter()
        .filter(|spec| query.is_empty() || spec.name.contains(&query))
        .collect()
}

#[cfg(test)]
pub(crate) fn command_spec(name: &str) -> Option<&'static CommandSpec> {
    COMMANDS
        .iter()
        .find(|spec| spec.name == name || spec.aliases.contains(&name))
}

#[cfg(test)]
pub(crate) fn parse_submission(input: &str) -> Result<Submission, String> {
    parse_submission_with_skills(input, &[])
}

pub(crate) fn parse_submission_with_skills(
    input: &str,
    skills: &[String],
) -> Result<Submission, String> {
    // Commands are deliberately single-line. Multiline text beginning with
    // `/` is always sent verbatim as a prompt, so a pasted block cannot
    // accidentally execute a client command.
    if input.contains('\n') {
        return Ok(Submission::Prompt(input.to_owned()));
    }
    if let Some(prompt) = input.strip_prefix("//") {
        return Ok(Submission::Prompt(format!("/{prompt}")));
    }
    let Some(command_line) = input.strip_prefix('/') else {
        return Ok(Submission::Prompt(input.to_owned()));
    };
    let parts = command_line.split_whitespace().collect::<Vec<_>>();
    if parts.first().is_none_or(|name| {
        !COMMANDS
            .iter()
            .any(|spec| spec.name == *name || spec.aliases.contains(name))
            && !skills.iter().any(|skill| skill == name)
    }) {
        return Err(format!("unknown command: /{command_line}"));
    }
    let command = if let Some(name) = parts
        .first()
        .filter(|name| skills.iter().any(|skill| skill == **name))
    {
        let args = command_line
            .strip_prefix(name)
            .map(str::trim)
            .unwrap_or_default()
            .to_owned();
        SlashCommand::Skill {
            name: (*name).to_owned(),
            args,
        }
    } else if parts.first() == Some(&"compact") {
        let focus = command_line
            .strip_prefix("compact")
            .map(str::trim)
            .filter(|focus| !focus.is_empty())
            .map(str::to_owned);
        SlashCommand::Compact(focus)
    } else {
        match parts.as_slice() {
            ["quit"] | ["q"] => SlashCommand::Quit,
            ["new"] => SlashCommand::New,
            ["connect"] => SlashCommand::Connect,
            ["mcp"] => SlashCommand::Mcp,
            ["permissions"] | ["perms"] => SlashCommand::Permissions,
            ["skills"] => SlashCommand::Skills,
            ["sessions"] => SlashCommand::Sessions,
            ["usage"] => SlashCommand::Usage,
            ["cancel"] => SlashCommand::Cancel,
            ["approve", "once"] => SlashCommand::Approve(ApprovalUserDecision::ApproveOnce),
            ["approve", "all"] => SlashCommand::Approve(ApprovalUserDecision::ApproveTree),
            ["approve", "reject"] => SlashCommand::Approve(ApprovalUserDecision::Reject),
            ["approve", "cancel"] => SlashCommand::Approve(ApprovalUserDecision::Cancel),
            ["events", "debug"] => SlashCommand::Events(crate::state::EventLevel::Debug),
            ["events", "info"] => SlashCommand::Events(crate::state::EventLevel::Info),
            ["events", "warning"] => SlashCommand::Events(crate::state::EventLevel::Warning),
            ["events", "error"] => SlashCommand::Events(crate::state::EventLevel::Error),
            ["help"] => SlashCommand::Help,
            _ => return Err(format!("invalid command: /{command_line}")),
        }
    };
    Ok(Submission::Command(command))
}

#[cfg(test)]
pub(crate) fn command_help() -> String {
    COMMANDS
        .iter()
        .map(|spec| format!("{} — {}", spec.usage, spec.description))
        .collect::<Vec<_>>()
        .join("; ")
}

#[cfg(test)]
mod parse_tests {
    use super::{SlashCommand, Submission, parse_submission, parse_submission_with_skills};

    #[test]
    fn compact_accepts_an_optional_focus() {
        assert_eq!(
            parse_submission("/compact").unwrap(),
            Submission::Command(SlashCommand::Compact(None))
        );
        assert_eq!(
            parse_submission("/compact preserve parser decisions").unwrap(),
            Submission::Command(SlashCommand::Compact(Some(
                "preserve parser decisions".into()
            )))
        );
    }

    #[test]
    fn parses_skills_panel_and_dynamic_skill_invocation() {
        assert_eq!(
            parse_submission("/skills").unwrap(),
            Submission::Command(SlashCommand::Skills)
        );
        assert_eq!(
            parse_submission_with_skills(
                "/release-check v1.2.0 --strict",
                &["release-check".into()]
            )
            .unwrap(),
            Submission::Command(SlashCommand::Skill {
                name: "release-check".into(),
                args: "v1.2.0 --strict".into(),
            })
        );
    }
}

/// One readable line per command for the in-transcript help notice.
pub(crate) fn command_help_lines() -> impl Iterator<Item = String> {
    COMMANDS
        .iter()
        .map(|spec| format!("{} — {}", spec.usage, spec.description))
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

pub(crate) fn render(
    frame: &mut Frame,
    query: &str,
    entries: Vec<String>,
    area: Rect,
    state: &mut ListState,
    theme: &crate::theme::Theme,
) -> Vec<(Rect, usize)> {
    let inner = inner_rect(area);
    let list_area = Rect::new(
        inner.x,
        inner.y.saturating_add(2),
        inner.width,
        inner.height.saturating_sub(2),
    );
    super::app::paint_panel(frame, area, theme);
    frame.render_widget(
        Block::default()
            .borders(Borders::ALL)
            .border_style(theme.panel_border())
            .title("Commands")
            .title_bottom(
                ratatui::text::Line::from(ratatui::text::Span::styled(
                    "↑↓ move · enter: choose · esc: dismiss",
                    theme.internal(),
                ))
                .right_aligned(),
            ),
        area,
    );
    frame.render_widget(
        Paragraph::new(format!("/{query}")).block(
            Block::default()
                .borders(Borders::BOTTOM)
                .border_style(theme.panel_border())
                .title("Search"),
        ),
        Rect::new(inner.x, inner.y, inner.width, inner.height.min(2)),
    );
    let entry_count = entries.len();
    if entry_count == 0 {
        frame.render_widget(
            Paragraph::new("No matching commands").style(theme.muted()),
            list_area,
        );
        state.select(None);
        return Vec::new();
    }
    // Rows ellipsize instead of hard-clipping at the panel edge; the two
    // selection-marker columns stay reserved on every row.
    let available = usize::from(list_area.width.saturating_sub(2));
    let entries = entries
        .into_iter()
        .map(|entry| super::app::truncate_with_ellipsis(&entry, available))
        .collect::<Vec<_>>();
    frame.render_stateful_widget(
        List::new(entries.into_iter().map(ListItem::new).collect::<Vec<_>>())
            .highlight_symbol("> ")
            .highlight_style(theme.selected()),
        list_area,
        state,
    );
    (state.offset()..entry_count)
        .take(usize::from(list_area.height))
        .enumerate()
        .map(|(row, index)| {
            (
                Rect::new(
                    list_area.x,
                    list_area.y + u16::try_from(row).unwrap_or(u16::MAX),
                    list_area.width,
                    1,
                ),
                index,
            )
        })
        .collect()
}

fn inner_rect(area: Rect) -> Rect {
    Rect::new(
        area.x.saturating_add(1),
        area.y.saturating_add(1),
        area.width.saturating_sub(2),
        area.height.saturating_sub(2),
    )
}

#[cfg(test)]
mod tests {
    use ratatui::{Terminal, backend::TestBackend, widgets::ListState};

    #[test]
    fn palette_rows_ellipsize_and_the_footer_explains_the_keys() {
        let theme = crate::theme::Theme::default();
        let mut terminal = Terminal::new(TestBackend::new(34, 10)).expect("terminal");
        let mut state = ListState::default().with_selected(Some(0));
        terminal
            .draw(|frame| {
                super::render(
                    frame,
                    "",
                    vec![
                        "/approve once|tree|reject|cancel — answer an approval".to_owned(),
                        "/quit — exit the TUI".to_owned(),
                    ],
                    frame.area(),
                    &mut state,
                    &theme,
                );
            })
            .expect("render palette");
        let buffer = terminal.backend().buffer();
        let text = (0..10)
            .flat_map(|y| (0..34).map(move |x| buffer[(x, y)].symbol().to_owned()))
            .collect::<String>();
        assert!(text.contains('…'), "{text}");
        assert!(!text.contains("answer an approval"), "{text}");
        // No row spills over the right border.
        assert_eq!(buffer[(33, 3)].symbol(), "│");
        assert!(text.contains("enter: choose"), "{text}");
        assert!(text.contains("esc: dismiss"), "{text}");
    }
}
