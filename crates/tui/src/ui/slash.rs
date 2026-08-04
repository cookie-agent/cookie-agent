//! Slash-command palette presentation and command definitions.

use cookie_agent_protocol::ApprovalUserDecision;
use ratatui::{
    Frame,
    layout::Rect,
    widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph},
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum InputMode {
    Message,
    ToolStdin,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum SlashCommand {
    Quit,
    New,
    Connect,
    Sessions,
    Cancel,
    Stdin { next: bool },
    Eof,
    Message,
    Watch,
    TreeUp,
    TreeDown,
    TreeToggle,
    Approve(ApprovalUserDecision),
    Scroll(ScrollCommand),
    Block(BlockCommand),
    Events(crate::state::EventLevel),
    Help,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ScrollCommand {
    Up(usize),
    Down(usize),
    Top,
    Bottom,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum BlockCommand {
    Next,
    Previous,
    Toggle,
    Clear,
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
        name: "sessions",
        aliases: &[],
        usage: "/sessions",
        description: "choose a session",
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
        name: "stdin",
        aliases: &[],
        usage: "/stdin [next]",
        description: "enter or cycle tool stdin",
        requires_arguments: true,
    },
    CommandSpec {
        name: "eof",
        aliases: &[],
        usage: "/eof",
        description: "close tool stdin",
        requires_arguments: false,
    },
    CommandSpec {
        name: "message",
        aliases: &[],
        usage: "/message",
        description: "leave tool stdin",
        requires_arguments: false,
    },
    CommandSpec {
        name: "watch",
        aliases: &[],
        usage: "/watch",
        description: "watch the selected tree session",
        requires_arguments: false,
    },
    CommandSpec {
        name: "tree",
        aliases: &[],
        usage: "/tree up|down|toggle",
        description: "navigate the session tree",
        requires_arguments: true,
    },
    CommandSpec {
        name: "approve",
        aliases: &[],
        usage: "/approve once|tree|reject|cancel",
        description: "answer an approval",
        requires_arguments: true,
    },
    CommandSpec {
        name: "scroll",
        aliases: &[],
        usage: "/scroll up|down [n]|top|bottom",
        description: "scroll the conversation",
        requires_arguments: true,
    },
    CommandSpec {
        name: "block",
        aliases: &[],
        usage: "/block next|previous|toggle|clear",
        description: "navigate thinking and tool blocks",
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

pub(crate) fn parse_submission(input: &str) -> Result<Submission, String> {
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
    }) {
        return Err(format!("unknown command: /{command_line}"));
    }
    let command = match parts.as_slice() {
        ["quit"] | ["q"] => SlashCommand::Quit,
        ["new"] => SlashCommand::New,
        ["connect"] => SlashCommand::Connect,
        ["sessions"] => SlashCommand::Sessions,
        ["cancel"] => SlashCommand::Cancel,
        ["stdin"] => SlashCommand::Stdin { next: false },
        ["stdin", "next"] => SlashCommand::Stdin { next: true },
        ["eof"] => SlashCommand::Eof,
        ["message"] => SlashCommand::Message,
        ["watch"] => SlashCommand::Watch,
        ["tree", "up"] => SlashCommand::TreeUp,
        ["tree", "down"] => SlashCommand::TreeDown,
        ["tree", "toggle"] => SlashCommand::TreeToggle,
        ["approve", "once"] => SlashCommand::Approve(ApprovalUserDecision::ApproveOnce),
        ["approve", "tree"] => SlashCommand::Approve(ApprovalUserDecision::ApproveTree),
        ["approve", "reject"] => SlashCommand::Approve(ApprovalUserDecision::Reject),
        ["approve", "cancel"] => SlashCommand::Approve(ApprovalUserDecision::Cancel),
        ["scroll", "up"] => SlashCommand::Scroll(ScrollCommand::Up(1)),
        ["scroll", "down"] => SlashCommand::Scroll(ScrollCommand::Down(1)),
        ["scroll", "top"] => SlashCommand::Scroll(ScrollCommand::Top),
        ["scroll", "bottom"] => SlashCommand::Scroll(ScrollCommand::Bottom),
        ["scroll", "up", count] => {
            SlashCommand::Scroll(ScrollCommand::Up(parse_scroll_count(count)?))
        }
        ["scroll", "down", count] => {
            SlashCommand::Scroll(ScrollCommand::Down(parse_scroll_count(count)?))
        }
        ["block", "next"] => SlashCommand::Block(BlockCommand::Next),
        ["block", "previous" | "prev"] => SlashCommand::Block(BlockCommand::Previous),
        ["block", "toggle"] => SlashCommand::Block(BlockCommand::Toggle),
        ["block", "clear"] => SlashCommand::Block(BlockCommand::Clear),
        ["events", "debug"] => SlashCommand::Events(crate::state::EventLevel::Debug),
        ["events", "info"] => SlashCommand::Events(crate::state::EventLevel::Info),
        ["events", "warning"] => SlashCommand::Events(crate::state::EventLevel::Warning),
        ["events", "error"] => SlashCommand::Events(crate::state::EventLevel::Error),
        ["help"] => SlashCommand::Help,
        _ => return Err(format!("invalid command: /{command_line}")),
    };
    Ok(Submission::Command(command))
}

fn parse_scroll_count(count: &str) -> Result<usize, String> {
    count
        .parse::<usize>()
        .ok()
        .filter(|count| *count > 0)
        .ok_or_else(|| format!("scroll count must be a positive integer: {count}"))
}

pub(crate) fn command_allowed_in_mode(command: SlashCommand, mode: InputMode) -> bool {
    match command {
        SlashCommand::Stdin { .. } => mode == InputMode::Message,
        SlashCommand::Eof | SlashCommand::Message => mode == InputMode::ToolStdin,
        _ => mode == InputMode::Message,
    }
}

pub(crate) fn command_name(command: SlashCommand) -> &'static str {
    match command {
        SlashCommand::Quit => "quit",
        SlashCommand::New => "new",
        SlashCommand::Connect => "connect",
        SlashCommand::Sessions => "sessions",
        SlashCommand::Cancel => "cancel",
        SlashCommand::Stdin { next: false } => "stdin",
        SlashCommand::Stdin { next: true } => "stdin next",
        SlashCommand::Eof => "eof",
        SlashCommand::Message => "message",
        SlashCommand::Watch => "watch",
        SlashCommand::TreeUp => "tree up",
        SlashCommand::TreeDown => "tree down",
        SlashCommand::TreeToggle => "tree toggle",
        SlashCommand::Approve(ApprovalUserDecision::ApproveOnce) => "approve once",
        SlashCommand::Approve(ApprovalUserDecision::ApproveTree) => "approve tree",
        SlashCommand::Approve(ApprovalUserDecision::Reject) => "approve reject",
        SlashCommand::Approve(ApprovalUserDecision::Cancel) => "approve cancel",
        SlashCommand::Scroll(_) => "scroll",
        SlashCommand::Block(_) => "block",
        SlashCommand::Events(crate::state::EventLevel::Debug) => "events debug",
        SlashCommand::Events(crate::state::EventLevel::Info) => "events info",
        SlashCommand::Events(crate::state::EventLevel::Warning) => "events warning",
        SlashCommand::Events(crate::state::EventLevel::Error) => "events error",
        SlashCommand::Help => "help",
    }
}

pub(crate) fn command_mode_name(command: SlashCommand) -> &'static str {
    match command {
        SlashCommand::Eof | SlashCommand::Message => "tool stdin",
        _ => "message",
    }
}

pub(crate) fn command_help() -> String {
    COMMANDS
        .iter()
        .map(|spec| format!("{} — {}", spec.usage, spec.description))
        .collect::<Vec<_>>()
        .join("; ")
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
) -> Vec<(Rect, usize)> {
    let inner = inner_rect(area);
    let list_area = Rect::new(
        inner.x,
        inner.y.saturating_add(2),
        inner.width,
        inner.height.saturating_sub(2),
    );
    frame.render_widget(Clear, area);
    frame.render_widget(
        Block::default().borders(Borders::ALL).title("Commands"),
        area,
    );
    frame.render_widget(
        Paragraph::new(format!("/{query}"))
            .block(Block::default().borders(Borders::BOTTOM).title("Search")),
        Rect::new(inner.x, inner.y, inner.width, inner.height.min(2)),
    );
    let entry_count = entries.len();
    frame.render_stateful_widget(
        List::new(entries.into_iter().map(ListItem::new).collect::<Vec<_>>())
            .highlight_symbol("> "),
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
