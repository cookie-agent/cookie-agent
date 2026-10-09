//! The `/permissions` panel: the selected session's effective rules as one
//! table grouped by action, edited in place through session overrides.

use cookie_agent_protocol::{
    PermissionAction, PermissionEffect, PermissionMode, PermissionRuleSource,
    SessionPermissionGetResult,
};
use ratatui::{
    Frame,
    layout::Rect,
    style::Style,
    text::{Line, Span},
    widgets::{ListState, Paragraph},
};
use unicode_width::UnicodeWidthStr;

use crate::theme::{DecisionTone, Theme};

use super::super::{
    app::{paint_panel, permission_mode_label, truncate_with_ellipsis},
    input::InputState,
    transcript::{approval_tool_icon, wrapped_line},
};
use super::action_label;

/// Every action in table order, which is also the order the engine reports.
pub(in crate::ui) const PERMISSION_ACTIONS: [PermissionAction; 9] = [
    PermissionAction::Read,
    PermissionAction::Write,
    PermissionAction::Bash,
    PermissionAction::Delegate,
    PermissionAction::Message,
    PermissionAction::Mcp,
    PermissionAction::Plugin,
    PermissionAction::Skill,
    PermissionAction::Webfetch,
];

const EFFECTS: [PermissionEffect; 3] = [
    PermissionEffect::Allow,
    PermissionEffect::Ask,
    PermissionEffect::Deny,
];

#[derive(Clone, Debug, Eq, PartialEq)]
pub(in crate::ui) struct PermissionRow {
    pub(in crate::ui) action: PermissionAction,
    pub(in crate::ui) resource: String,
    pub(in crate::ui) effect: PermissionEffect,
    pub(in crate::ui) source: PermissionRuleSource,
}

pub(in crate::ui) fn permission_rows(result: &SessionPermissionGetResult) -> Vec<PermissionRow> {
    result
        .permissions
        .iter()
        .flat_map(|action| {
            std::iter::once(PermissionRow {
                action: action.action,
                resource: "*".into(),
                effect: action.effect,
                source: action.source,
            })
            .chain(action.patterns.iter().map(|rule| PermissionRow {
                action: action.action,
                resource: rule.resource.as_str().to_owned(),
                effect: rule.effect,
                source: rule.source,
            }))
        })
        .collect()
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(in crate::ui) enum PermissionFormFocus {
    Action,
    Pattern,
    Effect,
}

/// The new-rule form, or the edit form of one session rule.
pub(in crate::ui) struct PermissionForm {
    pub(in crate::ui) action: PermissionAction,
    pub(in crate::ui) pattern: InputState,
    pub(in crate::ui) effect: PermissionEffect,
    pub(in crate::ui) focus: PermissionFormFocus,
    /// The session rule being edited, which a changed pattern replaces.
    pub(in crate::ui) editing: Option<(PermissionAction, String)>,
    /// Why the last submit was refused.
    pub(in crate::ui) error: Option<String>,
}

impl PermissionForm {
    pub(in crate::ui) fn new(action: PermissionAction) -> Self {
        Self {
            action,
            pattern: InputState::default(),
            effect: PermissionEffect::Ask,
            focus: PermissionFormFocus::Pattern,
            editing: None,
            error: None,
        }
    }

    pub(in crate::ui) fn edit(row: &PermissionRow) -> Self {
        let mut pattern = InputState::default();
        pattern.set_buffer(row.resource.clone());
        Self {
            action: row.action,
            pattern,
            effect: row.effect,
            focus: PermissionFormFocus::Pattern,
            editing: Some((row.action, row.resource.clone())),
            error: None,
        }
    }

    pub(in crate::ui) fn cycle_focus(&mut self, backward: bool) {
        use PermissionFormFocus::*;
        self.focus = match (self.focus, backward) {
            (Action, false) | (Effect, true) => Pattern,
            (Pattern, false) | (Action, true) => Effect,
            (Effect, false) | (Pattern, true) => Action,
        };
    }

    pub(in crate::ui) fn cycle_action(&mut self, backward: bool) {
        let index = PERMISSION_ACTIONS
            .iter()
            .position(|action| *action == self.action)
            .unwrap_or(0);
        let len = PERMISSION_ACTIONS.len();
        self.action = PERMISSION_ACTIONS[if backward {
            (index + len - 1) % len
        } else {
            (index + 1) % len
        }];
    }
}

#[derive(Default)]
pub(in crate::ui) struct PermissionPanel {
    pub(in crate::ui) result: Option<SessionPermissionGetResult>,
    /// The selected row; one past the last rule is the `new rule` row.
    pub(in crate::ui) selection: ListState,
    pub(in crate::ui) form: Option<PermissionForm>,
    /// First table row drawn, kept so the selection stays in view.
    offset: usize,
    /// A message about the last action, shown under the table.
    pub(in crate::ui) notice: Option<String>,
    /// Editing a new-session draft's rules, previewed against its agent,
    /// rather than a live session's overlay.
    pub(in crate::ui) draft: bool,
}

impl PermissionPanel {
    pub(in crate::ui) fn begin_load(&mut self) {
        self.result = None;
        self.form = None;
        self.notice = None;
        self.offset = 0;
        self.draft = false;
        self.selection.select(None);
    }

    pub(in crate::ui) fn rows(&self) -> Vec<PermissionRow> {
        self.result
            .as_ref()
            .map(permission_rows)
            .unwrap_or_default()
    }

    /// Selectable rows: every rule plus the trailing `new rule` row.
    pub(in crate::ui) fn row_count(&self) -> usize {
        if self.result.is_some() {
            self.rows().len() + 1
        } else {
            0
        }
    }

    pub(in crate::ui) fn selected(&self) -> Option<PermissionRow> {
        self.selection
            .selected()
            .and_then(|index| self.rows().get(index).cloned())
    }

    pub(in crate::ui) fn add_row_selected(&self) -> bool {
        self.result.is_some() && self.selection.selected() == Some(self.rows().len())
    }

    /// Install a fresh result, keeping the selection on the same rule when
    /// it still exists (a changed effect keeps its row).
    pub(in crate::ui) fn install(&mut self, result: SessionPermissionGetResult) {
        let previous = self.selected();
        self.result = Some(result);
        let rows = self.rows();
        let index = previous
            .and_then(|previous| {
                rows.iter().position(|row| {
                    row.action == previous.action && row.resource == previous.resource
                })
            })
            .or_else(|| self.selection.selected())
            .unwrap_or(0)
            .min(rows.len());
        self.selection.select(Some(index));
    }

    pub(in crate::ui) fn move_selection(&mut self, delta: isize) {
        let count = self.row_count();
        if count == 0 {
            return;
        }
        let current = self.selection.selected().unwrap_or(0);
        let next = current.saturating_add_signed(delta).min(count - 1);
        self.selection.select(Some(next));
    }

    pub(in crate::ui) fn select_last(&mut self) {
        if let Some(last) = self.row_count().checked_sub(1) {
            self.selection.select(Some(last));
        }
    }
}

/// One step through allow → ask → deny, stopping at either end the way a
/// segmented control does.
pub(in crate::ui) fn step_effect(effect: PermissionEffect, backward: bool) -> PermissionEffect {
    let index = EFFECTS
        .iter()
        .position(|candidate| *candidate == effect)
        .unwrap_or(1);
    let next = if backward {
        index.saturating_sub(1)
    } else {
        (index + 1).min(EFFECTS.len() - 1)
    };
    EFFECTS[next]
}

fn effect_label(effect: PermissionEffect) -> &'static str {
    match effect {
        PermissionEffect::Allow => "allow",
        PermissionEffect::Ask => "ask",
        PermissionEffect::Deny => "deny",
    }
}

fn effect_glyph(effect: PermissionEffect) -> &'static str {
    match effect {
        PermissionEffect::Allow => "✓",
        PermissionEffect::Ask => "?",
        PermissionEffect::Deny => "✗",
    }
}

fn effect_style(effect: PermissionEffect, active: bool, theme: &Theme) -> Style {
    match effect {
        PermissionEffect::Allow => theme.decision(DecisionTone::Allow, active),
        PermissionEffect::Deny => theme.decision(DecisionTone::Deny, active),
        PermissionEffect::Ask if active => theme.decision(DecisionTone::Neutral, true),
        PermissionEffect::Ask => theme.warning(),
    }
}

fn source_label(source: PermissionRuleSource) -> &'static str {
    match source {
        PermissionRuleSource::SessionOverlay => "session",
        PermissionRuleSource::AgentDocument => "agent",
        PermissionRuleSource::Default => "default",
    }
}

fn source_style(source: PermissionRuleSource, theme: &Theme) -> Style {
    match source {
        PermissionRuleSource::SessionOverlay => theme.user(),
        PermissionRuleSource::AgentDocument => theme.muted(),
        PermissionRuleSource::Default => theme.internal(),
    }
}

/// The transcript's tool icon for the tools an action gates.
fn action_icon(action: PermissionAction) -> &'static str {
    approval_tool_icon(match action {
        PermissionAction::Delegate => "delegate_subagent",
        PermissionAction::Message => "send_message",
        other => action_label(other),
    })
}

/// What an action's resource pattern matches, for the explanation line.
fn resource_noun(action: PermissionAction) -> &'static str {
    match action {
        PermissionAction::Read => "reads of paths",
        PermissionAction::Write => "writes to paths",
        PermissionAction::Bash => "bash commands",
        PermissionAction::Delegate => "delegations to agents",
        PermissionAction::Message => "messages to sessions",
        PermissionAction::Mcp => "MCP tools",
        PermissionAction::Plugin => "plugin tools",
        PermissionAction::Skill => "skills",
        PermissionAction::Webfetch => "fetches of URLs",
    }
}

fn effect_phrase(effect: PermissionEffect) -> &'static str {
    match effect {
        PermissionEffect::Allow => "run without asking",
        PermissionEffect::Ask => "ask you first",
        PermissionEffect::Deny => "are refused",
    }
}

/// The selected row in plain words: what it matches, what happens, where
/// it comes from, and what editing it does.
fn row_explanation(row: &PermissionRow, has_patterns: bool, draft: bool) -> String {
    let noun = resource_noun(row.action);
    let matched = if row.resource == "*" {
        if has_patterns {
            format!("Other {noun} (no pattern below matches)")
        } else {
            format!("All {noun}")
        }
    } else {
        format!("{} matching `{}`", capitalized(noun), row.resource)
    };
    let effect = effect_phrase(row.effect);
    // A draft's overrides wait for the session that the first prompt creates.
    let override_noun = if draft {
        "a rule for the new session"
    } else {
        "a session override"
    };
    let origin = match row.source {
        PermissionRuleSource::SessionOverlay => format!(
            "{}, checked before the agent's rules; d removes it, Enter edits it.",
            capitalized(override_noun)
        ),
        PermissionRuleSource::AgentDocument => {
            format!("From the agent; changing it adds {override_noun}.")
        }
        PermissionRuleSource::Default => format!(
            "No rule covers this, so it is denied and its tools are hidden; changing it adds \
             {override_noun}."
        ),
    };
    format!("{matched} {effect}. {origin}")
}

fn capitalized(text: &str) -> String {
    let mut characters = text.chars();
    characters.next().map_or_else(String::new, |first| {
        first.to_uppercase().chain(characters).collect()
    })
}

/// Where the panel's clickable parts landed this frame.
#[derive(Clone, Debug, Default)]
pub(in crate::ui) struct PermissionHits {
    /// (row rect, selectable row index); the last index is `new rule`.
    pub(in crate::ui) rows: Vec<(Rect, usize)>,
    /// The selected row's effect segments.
    pub(in crate::ui) effects: Vec<(Rect, PermissionEffect)>,
    /// The form's effect segments.
    pub(in crate::ui) form_effects: Vec<(Rect, PermissionEffect)>,
    pub(in crate::ui) mode: Option<Rect>,
}

const ACTION_COLUMNS: usize = 13;
const SOURCE_COLUMNS: usize = 9;

/// The effect column: a glyph and label, or on the selected row the full
/// segmented control whose segments are returned with their offsets.
fn effect_cells(
    effect: PermissionEffect,
    selected: bool,
    theme: &Theme,
) -> (Vec<Span<'static>>, Vec<(usize, usize, PermissionEffect)>) {
    if !selected {
        return (
            vec![Span::styled(
                format!("{} {}", effect_glyph(effect), effect_label(effect)),
                effect_style(effect, false, theme),
            )],
            Vec::new(),
        );
    }
    let mut spans = Vec::new();
    let mut segments = Vec::new();
    let mut column = 0;
    for (index, candidate) in EFFECTS.into_iter().enumerate() {
        if index > 0 {
            spans.push(Span::styled("│", theme.panel_border()));
            column += 1;
        }
        let text = format!(" {} ", effect_label(candidate));
        let width = text.len();
        let style = if candidate == effect {
            effect_style(candidate, true, theme)
        } else {
            theme.muted()
        };
        spans.push(Span::styled(text, style));
        segments.push((column, width, candidate));
        column += width;
    }
    (spans, segments)
}

const EFFECT_COLUMNS: usize = 23;

/// Render the panel centered in `screen`, sized to its rules, and return
/// where its clickable parts landed.
pub(in crate::ui) fn render_permissions(
    frame: &mut Frame,
    screen: Rect,
    panel: &mut PermissionPanel,
    mode: Option<PermissionMode>,
    theme: &Theme,
) -> PermissionHits {
    let mut hits = PermissionHits::default();
    let rows = panel.rows();
    // Sized to the longest pattern (plus its tree glyph and a gap), within
    // a readable minimum and nine tenths of the screen.
    let longest_pattern = rows
        .iter()
        .map(|row| UnicodeWidthStr::width(row.resource.as_str()))
        .max()
        .unwrap_or(0);
    let natural =
        4 + ACTION_COLUMNS + (longest_pattern + 4).max(18) + EFFECT_COLUMNS + SOURCE_COLUMNS;
    let width = u16::try_from(natural).unwrap_or(u16::MAX).clamp(
        screen.width.min(64),
        (screen.width.saturating_mul(9) / 10).max(screen.width.min(64)),
    );
    let explanation_width = width.saturating_sub(4);
    let explanation = panel
        .selected()
        .map(|row| {
            let has_patterns = rows
                .iter()
                .any(|other| other.action == row.action && other.resource != "*");
            row_explanation(&row, has_patterns, panel.draft)
        })
        .or_else(|| {
            panel.add_row_selected().then(|| {
                if panel.draft {
                    "Add a rule for the new session: it is checked before the agent's rules \
                     and applies from its first run."
                } else {
                    "Add a session rule: it is checked before the agent's rules and lasts for \
                     this session and its revert/fork branches."
                }
                .to_owned()
            })
        });
    let mut footer = Vec::new();
    if let Some(notice) = &panel.notice {
        footer.extend(wrapped_line(
            Line::styled(notice.clone(), theme.warning()),
            explanation_width,
        ));
    }
    if let Some(explanation) = explanation {
        footer.extend(wrapped_line(
            Line::styled(explanation, theme.muted_text()),
            explanation_width,
        ));
    }
    // The form is five rows under a one-row gap.
    let form_height = if panel.form.is_some() { 6 } else { 0 };
    let table_rows = panel.row_count().max(1);
    // Border, blank, header, table, blank, explanation, form, border.
    let desired = table_rows + footer.len() + form_height + 5;
    let height = u16::try_from(desired)
        .unwrap_or(u16::MAX)
        .min(screen.height.saturating_mul(9) / 10)
        .min(screen.height);
    let area = Rect::new(
        screen.x + screen.width.saturating_sub(width) / 2,
        screen.y + screen.height.saturating_sub(height) / 2,
        width,
        height,
    );
    paint_panel(frame, area, theme);

    let hints = if panel.form.is_some() {
        "tab field · ←→ change · ⏎ save · esc cancel"
    } else {
        "↑↓ select · ←→ effect · n new · d remove · m mode · esc close"
    };
    let mut block = crate::ui::panel_block()
        .border_style(theme.panel_border())
        .title(crate::ui::panel_title(Line::from(if panel.draft {
            vec![
                Span::styled("Permissions", theme.heading()),
                Span::styled(" · new session", theme.muted()),
            ]
        } else {
            vec![Span::styled("Permissions", theme.heading())]
        })))
        .style(theme.panel());
    let mode_text = mode.map(|mode| format!("mode {} · m", permission_mode_label(mode)));
    if let Some(text) = &mode_text {
        block = block.title(crate::ui::panel_title(
            Line::from(Span::styled(text.clone(), theme.muted())).right_aligned(),
        ));
    }
    if usize::from(area.width) > UnicodeWidthStr::width(hints) + 4 {
        block = block.title_bottom(crate::ui::panel_title(
            Line::from(Span::styled(hints, theme.internal())).right_aligned(),
        ));
    }
    frame.render_widget(block, area);
    if let Some(text) = mode_text {
        let text_width = UnicodeWidthStr::width(text.as_str()) as u16;
        let x = area.right().saturating_sub(2 + text_width);
        hits.mode = Some(Rect::new(x, area.y, text_width, 1));
    }

    let inner = Rect::new(
        area.x + 2,
        area.y + 1,
        area.width.saturating_sub(4),
        area.height.saturating_sub(2),
    );
    if inner.height < 2 {
        return hits;
    }
    if panel.result.is_none() {
        frame.render_widget(
            Paragraph::new(Span::styled("Loading permissions…", theme.muted())),
            Rect::new(inner.x, inner.y + 1, inner.width, 1),
        );
        return hits;
    }

    let pattern_columns = usize::from(inner.width)
        .saturating_sub(ACTION_COLUMNS + EFFECT_COLUMNS + SOURCE_COLUMNS)
        .max(8);
    let header = Line::from(vec![Span::styled(
        format!(
            "{:<ACTION_COLUMNS$}{:<pattern_columns$}{:<EFFECT_COLUMNS$}{}",
            "ACTION", "PATTERN", "EFFECT", "SOURCE"
        ),
        theme.internal(),
    )]);
    frame.render_widget(
        Paragraph::new(header),
        Rect::new(inner.x, inner.y + 1, inner.width, 1),
    );

    let footer_rows = u16::try_from(footer.len()).unwrap_or(u16::MAX);
    let table_top = inner.y + 2;
    let table_height = inner
        .height
        .saturating_sub(2 + footer_rows + 1 + form_height as u16);
    let visible = usize::from(table_height).max(1);
    let selected = panel.selection.selected().unwrap_or(0);
    if selected < panel.offset {
        panel.offset = selected;
    } else if selected >= panel.offset + visible {
        panel.offset = selected + 1 - visible;
    }
    panel.offset = panel.offset.min(panel.row_count().saturating_sub(visible));

    for (offset, index) in (panel.offset..panel.row_count()).take(visible).enumerate() {
        let y = table_top + offset as u16;
        let row_rect = Rect::new(inner.x.saturating_sub(1), y, inner.width + 2, 1);
        let is_selected = index == selected;
        if is_selected {
            let clip = row_rect.intersection(frame.area());
            frame.buffer_mut().set_style(clip, theme.selected_overlay());
        }
        let text_rect = Rect::new(inner.x, y, inner.width, 1);
        hits.rows.push((row_rect, index));
        let Some(row) = rows.get(index) else {
            frame.render_widget(
                Paragraph::new(Line::from(vec![
                    Span::styled("+ ", theme.assistant()),
                    Span::styled("new session rule", theme.assistant()),
                    Span::styled("  n", theme.internal()),
                ])),
                text_rect,
            );
            continue;
        };
        let first_of_action = index == 0 || rows[index - 1].action != row.action;
        let last_of_action = rows
            .get(index + 1)
            .is_none_or(|next| next.action != row.action);
        let mut spans = if first_of_action {
            vec![
                Span::raw(action_icon(row.action)),
                Span::raw(" "),
                Span::styled(
                    format!(
                        "{:<width$}",
                        action_label(row.action),
                        width = ACTION_COLUMNS - 3
                    ),
                    theme.heading(),
                ),
            ]
        } else {
            vec![Span::raw(" ".repeat(ACTION_COLUMNS))]
        };
        // An action's patterns hang off its `*` row as a small tree.
        let branch = if first_of_action {
            ""
        } else if last_of_action {
            "└ "
        } else {
            "├ "
        };
        spans.push(Span::styled(branch, theme.panel_border()));
        let pattern = truncate_with_ellipsis(
            &row.resource,
            pattern_columns.saturating_sub(2 + branch.chars().count()),
        );
        let pattern_width = UnicodeWidthStr::width(pattern.as_str()) + branch.chars().count();
        let pattern_style = if row.resource == "*" {
            theme.muted_text()
        } else {
            theme.body()
        };
        spans.push(Span::styled(pattern, pattern_style));
        spans.push(Span::raw(
            " ".repeat(pattern_columns.saturating_sub(pattern_width)),
        ));
        let (effect_spans, segments) = effect_cells(row.effect, is_selected, theme);
        let effect_width = effect_spans
            .iter()
            .map(|span| UnicodeWidthStr::width(span.content.as_ref()))
            .sum::<usize>();
        let effect_x = inner.x + (ACTION_COLUMNS + pattern_columns) as u16;
        hits.effects
            .extend(segments.into_iter().map(|(column, width, effect)| {
                (
                    Rect::new(effect_x + column as u16, y, width as u16, 1),
                    effect,
                )
            }));
        spans.extend(effect_spans);
        spans.push(Span::raw(
            " ".repeat(EFFECT_COLUMNS.saturating_sub(effect_width)),
        ));
        spans.push(Span::styled(
            source_label(row.source),
            source_style(row.source, theme),
        ));
        let mut line = Line::from(spans);
        if is_selected {
            for span in &mut line.spans {
                if span.style.bg.is_none() {
                    span.style = span.style.patch(theme.selected_overlay());
                }
            }
        }
        frame.render_widget(Paragraph::new(line), text_rect);
    }
    if panel.row_count() > visible {
        let label = format!(
            "{}–{}/{}",
            panel.offset + 1,
            (panel.offset + visible).min(panel.row_count()),
            panel.row_count()
        );
        // The row range sits in the bottom border, opposite the hints.
        let label_width = u16::try_from(label.len()).unwrap_or(u16::MAX);
        frame.render_widget(
            Paragraph::new(Span::styled(label, theme.internal())),
            Rect::new(
                area.x + 2,
                area.bottom() - 1,
                label_width.min(inner.width),
                1,
            ),
        );
    }

    let footer_top = table_top + table_height + 1;
    for (offset, line) in footer.into_iter().enumerate() {
        frame.render_widget(
            Paragraph::new(line),
            Rect::new(inner.x, footer_top + offset as u16, inner.width, 1),
        );
    }
    if let Some(form) = &mut panel.form {
        let form_area = Rect::new(
            inner.x,
            area.bottom().saturating_sub(form_height as u16),
            inner.width,
            form_height as u16 - 1,
        );
        hits.form_effects = render_form(frame, form_area, form, theme);
    }
    hits
}

/// The rule form: action, pattern, and effect on their own rows, the
/// focused one marked, any validation error in the bottom border.
fn render_form(
    frame: &mut Frame,
    area: Rect,
    form: &mut PermissionForm,
    theme: &Theme,
) -> Vec<(Rect, PermissionEffect)> {
    let title = if form.editing.is_some() {
        "Edit session rule"
    } else {
        "New session rule"
    };
    let mut block = crate::ui::panel_block()
        .border_style(theme.input_border(true))
        .title(crate::ui::panel_title(Span::styled(title, theme.heading())))
        .style(theme.panel());
    if let Some(error) = &form.error {
        block = block.title_bottom(crate::ui::panel_title(Span::styled(
            error.clone(),
            theme.error(),
        )));
    }
    frame.render_widget(block, area);
    let inner = Rect::new(
        area.x + 2,
        area.y + 1,
        area.width.saturating_sub(4),
        area.height.saturating_sub(2),
    );
    let label = |text: &'static str, focused: bool| {
        let style = if focused { theme.user() } else { theme.muted() };
        Span::styled(
            format!("{} {text:<9}", if focused { "›" } else { " " }),
            style,
        )
    };
    let rows = [
        PermissionFormFocus::Action,
        PermissionFormFocus::Pattern,
        PermissionFormFocus::Effect,
    ];
    let mut segments = Vec::new();
    for (offset, field) in rows.into_iter().enumerate() {
        let y = inner.y + offset as u16;
        if y >= inner.bottom() {
            break;
        }
        let focused = form.focus == field;
        let row = Rect::new(inner.x, y, inner.width, 1);
        let mut spans = vec![label(
            match field {
                PermissionFormFocus::Action => "action",
                PermissionFormFocus::Pattern => "pattern",
                PermissionFormFocus::Effect => "effect",
            },
            focused,
        )];
        let label_width = 11;
        match field {
            PermissionFormFocus::Action => {
                let arrows = if focused {
                    theme.user()
                } else {
                    theme.internal()
                };
                spans.extend([
                    Span::styled("‹ ", arrows),
                    Span::raw(action_icon(form.action)),
                    Span::raw(" "),
                    Span::styled(action_label(form.action), theme.heading()),
                    Span::styled(" ›", arrows),
                ]);
            }
            PermissionFormFocus::Pattern => {
                let text = form.pattern.as_str();
                if text.is_empty() {
                    spans.push(Span::styled(
                        match form.action {
                            PermissionAction::Bash => "e.g. git log*",
                            PermissionAction::Webfetch => "e.g. https://docs.rs/*",
                            PermissionAction::Delegate | PermissionAction::Message => {
                                "e.g. explore"
                            }
                            _ => "e.g. src/** or *",
                        },
                        theme.internal(),
                    ));
                } else {
                    spans.push(Span::styled(text.to_owned(), theme.body()));
                }
                if focused {
                    let cursor = form.pattern.cursor_visual_position(u16::MAX).1;
                    frame.set_cursor_position((
                        row.x + label_width + cursor.min(row.width.saturating_sub(label_width + 1)),
                        y,
                    ));
                }
            }
            PermissionFormFocus::Effect => {
                let (effect_spans, effect_segments) = effect_cells(form.effect, true, theme);
                let x = row.x + label_width;
                segments.extend(effect_segments.into_iter().map(|(column, width, effect)| {
                    (Rect::new(x + column as u16, y, width as u16, 1), effect)
                }));
                spans.extend(effect_spans);
            }
        }
        frame.render_widget(Paragraph::new(Line::from(spans)), row);
    }
    segments
}

/// A realistic effective rule set: agent rules with patterns, a session
/// override on a pattern and on a whole action, and uncovered defaults.
#[cfg(test)]
pub(in crate::ui) fn sample_permissions() -> SessionPermissionGetResult {
    use PermissionEffect::{Allow, Ask, Deny};
    use PermissionRuleSource::{AgentDocument, Default, SessionOverlay};
    use cookie_agent_protocol::{
        EffectivePermissionAction, EffectivePermissionRule, WildcardPattern,
    };
    let rule = |resource: &str, effect, source| EffectivePermissionRule {
        resource: WildcardPattern::new(resource).expect("pattern"),
        effect,
        source,
    };
    let action = |action, effect, source, patterns| EffectivePermissionAction {
        action,
        effect,
        source,
        patterns,
    };
    SessionPermissionGetResult {
        permissions: vec![
            action(
                PermissionAction::Read,
                Allow,
                AgentDocument,
                vec![
                    rule("*.env", Deny, AgentDocument),
                    rule("artifact://*", Allow, AgentDocument),
                ],
            ),
            action(
                PermissionAction::Write,
                Ask,
                AgentDocument,
                vec![rule("docs/*", Allow, SessionOverlay)],
            ),
            action(
                PermissionAction::Bash,
                Ask,
                AgentDocument,
                vec![
                    rule("git diff*", Allow, AgentDocument),
                    rule("git status*", Allow, AgentDocument),
                    rule("rm -rf *", Deny, AgentDocument),
                ],
            ),
            action(PermissionAction::Delegate, Allow, AgentDocument, vec![]),
            action(PermissionAction::Message, Deny, Default, vec![]),
            action(PermissionAction::Mcp, Ask, AgentDocument, vec![]),
            action(PermissionAction::Plugin, Deny, Default, vec![]),
            action(PermissionAction::Skill, Allow, AgentDocument, vec![]),
            action(PermissionAction::Webfetch, Ask, SessionOverlay, vec![]),
        ],
        current_mode: None,
    }
}
