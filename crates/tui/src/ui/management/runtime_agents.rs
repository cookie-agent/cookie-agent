//! A runtime's ACP agents, opened from the machines view (`a`): its
//! `[runtimes.<id>.acp.agents]` declarations, their form, and the documents
//! that run them (`docs/site/specs/runtime-management.md` section 4.2).

use cookie_agent_client_state::runtimes::AcpAgentFields;
use cookie_agent_protocol::{AgentId, RuntimeAgentDeclaration, RuntimeId};
use ratatui::{
    Frame,
    layout::{Constraint, Direction, Layout, Rect},
    text::{Line, Span},
    widgets::{List, ListItem, ListState, Paragraph, Wrap},
};

use crate::{theme::Theme, ui::input::InputState};

/// The agents view of one runtime.
pub(in crate::ui) struct AgentsView {
    pub(in crate::ui) runtime: RuntimeId,
    /// `runtimes.agents.list`, or why it failed; `None` until it answers.
    pub(in crate::ui) declarations: Option<Result<Vec<RuntimeAgentDeclaration>, String>>,
    pub(in crate::ui) selection: ListState,
    pub(in crate::ui) form: Option<AcpAgentForm>,
}

impl AgentsView {
    pub(in crate::ui) fn new(runtime: RuntimeId) -> Self {
        Self {
            runtime,
            declarations: None,
            selection: ListState::default(),
            form: None,
        }
    }

    pub(in crate::ui) fn install(
        &mut self,
        declarations: Result<Vec<RuntimeAgentDeclaration>, String>,
    ) {
        let count = declarations.as_ref().map_or(0, Vec::len);
        self.declarations = Some(declarations);
        self.selection.select((count > 0).then(|| {
            self.selection
                .selected()
                .unwrap_or(0)
                .min(count.saturating_sub(1))
        }));
    }

    pub(in crate::ui) fn selected(&self) -> Option<&RuntimeAgentDeclaration> {
        let declarations = self.declarations.as_ref()?.as_ref().ok()?;
        declarations.get(self.selection.selected()?)
    }

    pub(in crate::ui) fn move_selection(&mut self, up: bool) {
        let count = self
            .declarations
            .as_ref()
            .and_then(|declarations| declarations.as_ref().ok())
            .map_or(0, Vec::len);
        if count == 0 {
            return;
        }
        let selected = self.selection.selected().unwrap_or(0);
        self.selection.select(Some(if up {
            selected.saturating_sub(1)
        } else {
            (selected + 1).min(count - 1)
        }));
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(in crate::ui) enum AcpFormFocus {
    Agent,
    Command,
    Environment,
    Description,
    Enabled,
    Submit,
}

const FOCUS_ORDER: [AcpFormFocus; 6] = [
    AcpFormFocus::Agent,
    AcpFormFocus::Command,
    AcpFormFocus::Environment,
    AcpFormFocus::Description,
    AcpFormFocus::Enabled,
    AcpFormFocus::Submit,
];

/// The form of one declaration.
pub(in crate::ui) struct AcpAgentForm {
    /// The key of the declaration it edits; a changed key renames it.
    pub(in crate::ui) editing: Option<AgentId>,
    agent: InputState,
    command: InputState,
    environment: InputState,
    description: InputState,
    pub(in crate::ui) enabled: bool,
    pub(in crate::ui) focus: AcpFormFocus,
}

fn input(value: &str) -> InputState {
    let mut input = InputState::default();
    input.set_buffer(value.to_owned());
    input
}

impl AcpAgentForm {
    pub(in crate::ui) fn new(fields: &AcpAgentFields, editing: Option<AgentId>) -> Self {
        Self {
            editing,
            agent: input(&fields.agent),
            command: input(&fields.command),
            environment: input(&fields.environment),
            description: input(&fields.description),
            enabled: fields.enabled,
            focus: AcpFormFocus::Agent,
        }
    }

    pub(in crate::ui) fn fields(&self) -> AcpAgentFields {
        AcpAgentFields {
            agent: self.agent.as_str().to_owned(),
            command: self.command.as_str().to_owned(),
            environment: self.environment.as_str().to_owned(),
            description: self.description.as_str().to_owned(),
            enabled: self.enabled,
        }
    }

    pub(in crate::ui) fn move_focus(&mut self, backward: bool) {
        let index = FOCUS_ORDER
            .iter()
            .position(|focus| *focus == self.focus)
            .unwrap_or(0);
        let count = FOCUS_ORDER.len();
        self.focus = FOCUS_ORDER[if backward {
            (index + count - 1) % count
        } else {
            (index + 1) % count
        }];
    }

    pub(in crate::ui) fn focused_input(&mut self) -> Option<&mut InputState> {
        match self.focus {
            AcpFormFocus::Agent => Some(&mut self.agent),
            AcpFormFocus::Command => Some(&mut self.command),
            AcpFormFocus::Environment => Some(&mut self.environment),
            AcpFormFocus::Description => Some(&mut self.description),
            AcpFormFocus::Enabled | AcpFormFocus::Submit => None,
        }
    }
}

/// The agents view: the form while one is open, else the declarations and
/// the selected one's command and documents.
pub(in crate::ui) fn render_agents(
    frame: &mut Frame,
    area: Rect,
    view: &mut AgentsView,
    theme: &Theme,
) {
    if let Some(form) = view.form.as_ref() {
        render_form(frame, area, &view.runtime, form, theme);
        return;
    }
    let [list_area, detail_area] = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(3), Constraint::Length(7)])
        .areas(area);
    let rows = match &view.declarations {
        None => vec![ListItem::new(Line::styled(
            "Loading ACP agents...",
            theme.muted(),
        ))],
        Some(Err(error)) => vec![ListItem::new(Line::styled(
            format!("ACP agents are unavailable: {error}"),
            theme.muted(),
        ))],
        Some(Ok(declarations)) if declarations.is_empty() => vec![ListItem::new(Line::styled(
            format!(
                "[runtimes.{}.acp.agents] declares none; n adds one.",
                view.runtime
            ),
            theme.muted(),
        ))],
        Some(Ok(declarations)) => declarations
            .iter()
            .map(|declaration| {
                let state = if !declaration.definition.enabled {
                    "disabled"
                } else if declaration.available {
                    "available"
                } else {
                    "unavailable"
                };
                ListItem::new(Line::from(vec![
                    Span::styled(format!("  {}", declaration.agent), theme.body()),
                    Span::styled(format!("  {state}"), theme.muted()),
                ]))
            })
            .collect(),
    };
    frame.render_stateful_widget(
        List::new(rows)
            .highlight_symbol("> ")
            .highlight_style(theme.selected())
            .block(
                crate::ui::panel_block()
                    .border_style(theme.panel_border())
                    .title(crate::ui::panel_title(format!("ACP agents on {}", view.runtime)))
                    .title_bottom(crate::ui::panel_title(
                        Line::from(Span::styled(
                            "n add | e edit | d remove | space toggle | o document | t template | esc machines",
                            theme.internal(),
                        ))
                        .right_aligned(),
                    )),
            ),
        list_area,
        &mut view.selection,
    );
    let detail = view.selected().map_or_else(String::new, |declaration| {
        let mut text = format!("command: {}", declaration.definition.command.join(" "));
        if !declaration.definition.description.is_empty() {
            text.push_str(&format!(
                "\ndescription: {}",
                declaration.definition.description
            ));
        }
        if !declaration.definition.env.is_empty() {
            let names = declaration
                .definition
                .env
                .keys()
                .map(String::as_str)
                .collect::<Vec<_>>();
            text.push_str(&format!("\nenvironment: {}", names.join(", ")));
        }
        text.push_str(&if declaration.documents.is_empty() {
            "\ndocuments: none yet; o creates one".to_owned()
        } else {
            format!("\ndocuments: {}", declaration.documents.join(", "))
        });
        text
    });
    frame.render_widget(
        Paragraph::new(detail).wrap(Wrap { trim: false }).block(
            crate::ui::panel_block()
                .border_style(theme.panel_border())
                .title(crate::ui::panel_title("Details")),
        ),
        detail_area,
    );
}

fn render_form(
    frame: &mut Frame,
    area: Rect,
    runtime: &RuntimeId,
    form: &AcpAgentForm,
    theme: &Theme,
) {
    let rows = [
        (
            AcpFormFocus::Agent,
            "Agent ID",
            form.agent.as_str().to_owned(),
        ),
        (
            AcpFormFocus::Command,
            "Command JSON array",
            form.command.as_str().to_owned(),
        ),
        (
            AcpFormFocus::Environment,
            "Env JSON object",
            form.environment.as_str().to_owned(),
        ),
        (
            AcpFormFocus::Description,
            "Description",
            form.description.as_str().to_owned(),
        ),
        (AcpFormFocus::Enabled, "Enabled", form.enabled.to_string()),
        (AcpFormFocus::Submit, "Submit", "press enter".into()),
    ];
    let mut lines = rows
        .into_iter()
        .map(|(focus, label, value)| {
            let marker = if form.focus == focus { ">" } else { " " };
            Line::from(vec![
                Span::styled(format!("{marker} {label}: "), theme.muted()),
                Span::styled(
                    value,
                    if form.focus == focus {
                        theme.selected()
                    } else {
                        theme.body()
                    },
                ),
            ])
        })
        .collect::<Vec<_>>();
    if form.editing.is_some() {
        lines.push(Line::styled(
            "  A new ID renames the declaration, and its document when no other runtime declares the old ID.",
            theme.muted(),
        ));
    }
    let title = match &form.editing {
        Some(agent) => format!("Edit ACP agent {agent} on {runtime}"),
        None => format!("New ACP agent on {runtime}"),
    };
    frame.render_widget(
        Paragraph::new(lines).wrap(Wrap { trim: false }).block(
            crate::ui::panel_block()
                .border_style(theme.panel_border())
                .title(crate::ui::panel_title(title))
                .title_bottom(crate::ui::panel_title(
                    Line::from(Span::styled(
                        "tab next | space toggle | enter save | esc cancel",
                        theme.internal(),
                    ))
                    .right_aligned(),
                )),
        ),
        area,
    );
}
