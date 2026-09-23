//! Command palette and agent/model/session picker handling.

use super::*;

/// The exact `Agent • Model[Variant]` draft-selection title form.
pub(super) fn draft_title(draft: &RunSelection) -> String {
    let variant = draft
        .model
        .variant
        .as_ref()
        .map_or_else(|| "base".to_owned(), |variant| variant.to_string());
    format!("{} • {}[{}]", draft.agent, draft.model.model, variant)
}

pub(super) fn agent_picker_row(
    agent: &AgentDescriptor,
    width: usize,
    selected: bool,
    theme: &Theme,
) -> Line<'static> {
    let id = truncate_with_ellipsis(agent.id.as_str(), width);
    let id_width = UnicodeWidthStr::width(id.as_str());
    let id_style = theme.body().add_modifier(Modifier::BOLD);
    let id_style = if selected {
        id_style.patch(theme.selected())
    } else {
        id_style
    };
    let mut spans = vec![Span::styled(id, id_style)];
    if !agent.description.trim().is_empty() && id_width < width {
        let description_style = if selected {
            theme.internal().patch(theme.selected_overlay())
        } else {
            theme.internal()
        };
        spans.push(Span::styled(
            truncate_with_ellipsis(
                &format!(" {}", agent.description),
                width.saturating_sub(id_width),
            ),
            description_style,
        ));
    }
    Line::from(spans)
}

pub(super) fn model_picker_row(
    selection: &ModelSelection,
    display_name: Option<&str>,
    width: usize,
    selected: bool,
    theme: &Theme,
) -> Line<'static> {
    let variant = selection.variant.as_ref().map_or("base", VariantId::as_str);
    let canonical = format!("{}[{variant}]", selection.model);
    let Some(display_name) = display_name else {
        let style = if selected {
            theme.body().patch(theme.selected())
        } else {
            theme.body()
        };
        return Line::from(Span::styled(
            truncate_with_ellipsis(&canonical, width),
            style,
        ));
    };

    let display = truncate_with_ellipsis(display_name, width);
    let display_width = UnicodeWidthStr::width(display.as_str());
    let display_style = theme.body().add_modifier(Modifier::BOLD);
    let display_style = if selected {
        display_style.patch(theme.selected())
    } else {
        display_style
    };
    let mut spans = vec![Span::styled(display, display_style)];
    if display_width < width {
        let key_style = if selected {
            theme.internal().patch(theme.selected_overlay())
        } else {
            theme.internal()
        };
        spans.push(Span::styled(
            truncate_with_ellipsis(&format!(" {canonical}"), width - display_width),
            key_style,
        ));
    }
    Line::from(spans)
}

/// Whether a key edited a palette field's text (as opposed to moving the
/// cursor or doing nothing).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum LineEdit {
    Changed,
    Unchanged,
}

/// Single-line editing shared by every palette field.
fn edit_line(input: &mut InputState, key: KeyEvent) -> LineEdit {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    match key.code {
        KeyCode::Backspace if ctrl => input.delete_word_left(),
        KeyCode::Backspace => input.backspace(),
        KeyCode::Delete if ctrl => input.delete_word_right(),
        KeyCode::Delete => input.delete(),
        KeyCode::Char('u') if key.modifiers == KeyModifiers::CONTROL => {
            input.set_buffer(String::new());
        }
        KeyCode::Char(character) if is_printable_key(key) => input.insert(character),
        code => {
            match code {
                KeyCode::Left if ctrl => input.move_word_left(),
                KeyCode::Left => input.move_left(),
                KeyCode::Right if ctrl => input.move_word_right(),
                KeyCode::Right => input.move_right(),
                KeyCode::Home => input.move_buffer_home(),
                KeyCode::End => input.move_buffer_end(),
                KeyCode::Char('a') if key.modifiers == KeyModifiers::CONTROL => {
                    input.move_buffer_home();
                }
                KeyCode::Char('e') if key.modifiers == KeyModifiers::CONTROL => {
                    input.move_buffer_end();
                }
                _ => {}
            }
            return LineEdit::Unchanged;
        }
    }
    LineEdit::Changed
}

fn skill_label(skill: &cookie_agent_protocol::SkillDescriptor) -> String {
    let hint = skill
        .argument_hint
        .as_deref()
        .map_or(String::new(), |hint| format!(" {hint}"));
    format!("/{}{} — {}", skill.name, hint, skill.description)
}

/// What Enter on the palette's current step acts on, captured before any
/// state changes.
enum PaletteSubmit {
    Command,
    EventLevel,
    Skill,
    Text(TextTarget, String),
}

impl App {
    pub(in crate::ui) fn command_palette_visible(&self) -> bool {
        self.palette.is_some()
    }

    /// Open the palette on an empty command search. The composer draft is
    /// left exactly as it was.
    pub(in crate::ui) fn open_command_palette(&mut self) {
        self.palette = Some(CommandPalette::new());
    }

    pub(in crate::ui) fn close_command_palette(&mut self) {
        self.palette = None;
    }

    fn push_palette_step(&mut self, step: PaletteStep) {
        if let Some(palette) = self.palette.as_mut() {
            palette.steps.push(step);
        }
    }

    fn palette_step(&self) -> Option<&PaletteStep> {
        self.palette
            .as_ref()
            .and_then(|palette| palette.steps.last())
    }

    /// Commands matching the palette's search, best match first.
    pub(in crate::ui) fn palette_commands(&self) -> Vec<&'static CommandSpec> {
        let Some(palette) = &self.palette else {
            return Vec::new();
        };
        crate::ui::slash::entries(palette.search.as_str())
            .into_iter()
            .filter(|spec| self.command_is_available(spec))
            .collect()
    }

    /// User-invocable skills matching the skills step's search: name
    /// matches rank by exactness, then description-only matches.
    pub(in crate::ui) fn palette_skills(&self) -> Vec<&cookie_agent_protocol::SkillDescriptor> {
        let Some(PaletteStep::Skills { search, .. }) = self.palette_step() else {
            return Vec::new();
        };
        let query = crate::ui::slash::normalized_query(search.as_str());
        let mut ranked = self
            .skills
            .iter()
            .filter(|skill| skill.precedence_winner && skill.user_invocable)
            .filter_map(|skill| {
                crate::ui::slash::match_rank([skill.name.as_str()], &query)
                    .or_else(|| {
                        skill
                            .description
                            .to_lowercase()
                            .contains(&query)
                            .then_some(3)
                    })
                    .map(|rank| (rank, skill))
            })
            .collect::<Vec<_>>();
        ranked.sort_by_key(|(rank, _)| *rank);
        ranked.into_iter().map(|(_, skill)| skill).collect()
    }

    fn palette_list_len(&self) -> usize {
        match self.palette_step() {
            None => self.palette_commands().len(),
            Some(PaletteStep::EventLevel { .. }) => EVENT_LEVELS.len(),
            Some(PaletteStep::Skills { .. }) => self.palette_skills().len(),
            Some(PaletteStep::Text { .. }) => 0,
        }
    }

    /// Row labels of the current list step; empty for a text step.
    pub(in crate::ui) fn palette_list_labels(&self) -> Vec<String> {
        match self.palette_step() {
            None => self
                .palette_commands()
                .iter()
                .map(|spec| spec.label())
                .collect(),
            Some(PaletteStep::EventLevel { .. }) => EVENT_LEVELS
                .iter()
                .map(|level| {
                    if *level == self.tui_config.minimum_event_level {
                        format!("{} (current)", level.name())
                    } else {
                        level.name().to_owned()
                    }
                })
                .collect(),
            Some(PaletteStep::Skills { .. }) => {
                self.palette_skills().into_iter().map(skill_label).collect()
            }
            Some(PaletteStep::Text { .. }) => Vec::new(),
        }
    }

    /// Source, permission effect, and location of the highlighted skill.
    pub(in crate::ui) fn palette_skill_detail(&self) -> Option<String> {
        let Some(PaletteStep::Skills { list, .. }) = self.palette_step() else {
            return None;
        };
        let skills = self.palette_skills();
        let skill = skills.get(list.selected()?)?;
        Some(format!(
            "{:?} · permission {:?} · {}",
            skill.source, skill.permission_effect, skill.location
        ))
    }

    pub(in crate::ui) fn clamp_palette_selection(&mut self) {
        let len = self.palette_list_len();
        if let Some(list) = self.palette.as_mut().and_then(CommandPalette::active_list) {
            list.select((len > 0).then(|| list.selected().unwrap_or(0).min(len - 1)));
        }
    }

    pub(in crate::ui) fn move_palette_selection(&mut self, up: bool) {
        let len = self.palette_list_len();
        if let Some(list) = self.palette.as_mut().and_then(CommandPalette::active_list) {
            move_selection(list, len, up);
        }
    }

    pub(in crate::ui) async fn handle_palette_key(&mut self, key: KeyEvent) {
        let Some(palette) = self.palette.as_mut() else {
            return;
        };
        // Palette fields are single-line; newline chords do nothing.
        if is_newline_key(key) {
            return;
        }
        match key.code {
            KeyCode::Esc => {
                self.last_escape = None;
                if palette.steps.pop().is_some() {
                    self.clamp_palette_selection();
                } else {
                    self.close_command_palette();
                }
                return;
            }
            KeyCode::Enter => {
                let index = palette.active_list().and_then(|list| list.selected());
                self.submit_palette_step(index).await;
                return;
            }
            KeyCode::Up | KeyCode::Down if palette.active_list().is_some() => {
                self.move_palette_selection(key.code == KeyCode::Up);
                return;
            }
            // `/` in an empty command search is the way to start a prompt
            // with a literal slash.
            KeyCode::Char('/')
                if is_printable_key(key)
                    && palette.steps.is_empty()
                    && palette.search.as_str().is_empty() =>
            {
                self.close_command_palette();
                self.mutate_input(|input| input.insert('/'));
                return;
            }
            _ => {}
        }
        let Some(input) = palette.active_input() else {
            return;
        };
        if edit_line(input, key) == LineEdit::Changed {
            // A changed filter starts again from its best match.
            if let Some(list) = palette.active_list() {
                list.select(Some(0));
            }
            self.clamp_palette_selection();
        }
    }

    /// Paste into the palette's current field as a single line.
    pub(in crate::ui) fn paste_into_palette(&mut self, text: &str) {
        let Some(palette) = self.palette.as_mut() else {
            return;
        };
        let Some(input) = palette.active_input() else {
            return;
        };
        input.insert_text(&text.replace("\r\n", " ").replace(['\r', '\n'], " "));
        if let Some(list) = palette.active_list() {
            list.select(Some(0));
        }
        self.clamp_palette_selection();
    }

    /// Choose row `index` of the current list step, as a click does.
    pub(in crate::ui) async fn activate_palette_row(&mut self, index: usize) {
        if let Some(list) = self.palette.as_mut().and_then(CommandPalette::active_list) {
            list.select(Some(index));
        }
        self.submit_palette_step(Some(index)).await;
    }

    /// Enter on the current step: choose the highlighted row, or submit the
    /// text step's input.
    async fn submit_palette_step(&mut self, index: Option<usize>) {
        let submit = match self.palette_step() {
            None => PaletteSubmit::Command,
            Some(PaletteStep::EventLevel { .. }) => PaletteSubmit::EventLevel,
            Some(PaletteStep::Skills { .. }) => PaletteSubmit::Skill,
            Some(PaletteStep::Text { target, input }) => {
                PaletteSubmit::Text(target.clone(), input.as_str().trim().to_owned())
            }
        };
        match submit {
            PaletteSubmit::Command => {
                let Some(spec) =
                    index.and_then(|index| self.palette_commands().get(index).copied())
                else {
                    self.status = "no matching command".into();
                    return;
                };
                self.choose_palette_command(spec).await;
            }
            PaletteSubmit::EventLevel => {
                let Some(level) = index.and_then(|index| EVENT_LEVELS.get(index).copied()) else {
                    return;
                };
                self.close_command_palette();
                self.set_event_level(level);
            }
            PaletteSubmit::Skill => {
                let Some((name, hint)) = index.and_then(|index| {
                    self.palette_skills()
                        .get(index)
                        .map(|skill| (skill.name.clone(), skill.argument_hint.clone()))
                }) else {
                    self.status = "no matching skill".into();
                    return;
                };
                self.push_palette_step(PaletteStep::Text {
                    target: TextTarget::SkillArguments { name, hint },
                    input: InputState::default(),
                });
            }
            PaletteSubmit::Text(TextTarget::GoalObjective, objective) => {
                if objective.is_empty() {
                    self.status = "goal objective must not be empty".into();
                    return;
                }
                self.close_command_palette();
                self.run_goal_command(GoalCommand::Objective(objective));
            }
            PaletteSubmit::Text(TextTarget::CompactFocus, focus) => {
                self.close_command_palette();
                self.compact_selected_session((!focus.is_empty()).then_some(focus))
                    .await;
            }
            PaletteSubmit::Text(TextTarget::SkillArguments { name, .. }, args) => {
                self.close_command_palette();
                self.invoke_skill(name, args).await;
            }
        }
    }

    pub(in crate::ui) async fn choose_palette_command(&mut self, spec: &'static CommandSpec) {
        match spec.action {
            PaletteAction::Run(command) => {
                self.close_command_palette();
                self.run_command(command).await;
            }
            PaletteAction::Goal => {
                // Refuse up front rather than after the objective is typed.
                if let Some(reason) = self.goal_command_blocker() {
                    self.status = reason.into();
                    return;
                }
                self.push_palette_step(PaletteStep::Text {
                    target: TextTarget::GoalObjective,
                    input: InputState::default(),
                });
            }
            PaletteAction::Compact => self.push_palette_step(PaletteStep::Text {
                target: TextTarget::CompactFocus,
                input: InputState::default(),
            }),
            PaletteAction::Events => {
                let current = EVENT_LEVELS
                    .iter()
                    .position(|level| *level == self.tui_config.minimum_event_level);
                self.push_palette_step(PaletteStep::EventLevel {
                    list: ListState::default().with_selected(current.or(Some(0))),
                });
            }
            PaletteAction::Skills => {
                if self.load_palette_skills().await {
                    self.push_palette_step(PaletteStep::Skills {
                        search: InputState::default(),
                        list: ListState::default().with_selected(Some(0)),
                    });
                    self.clamp_palette_selection();
                }
            }
        }
    }

    /// Refresh the selected session's skills before the skills step opens.
    /// A pending new-session draft with nothing selected has no session to
    /// ask, so its root session is created first.
    async fn load_palette_skills(&mut self) -> bool {
        if self.selected.is_none()
            && let Some(selection) = self.new_session_draft.clone()
            && !self.create_root_session(selection).await
        {
            return false;
        }
        let Some(session_id) = self.selected else {
            self.status = "select a session before listing skills".into();
            return false;
        };
        match self
            .client
            .list_skills(cookie_agent_protocol::SkillsListParams { session_id })
            .await
        {
            Ok(result) => {
                self.skills = result.skills;
                true
            }
            Err(error) => {
                self.status = format!("list skills failed: {error}");
                false
            }
        }
    }

    pub(in crate::ui) async fn handle_session_picker(&mut self, key: KeyEvent) {
        let count = self.session_search_ids().len();
        if self.session_search.focus() == SearchPickerFocus::Input {
            match key.code {
                KeyCode::Esc => {
                    self.session_search.reset();
                    self.modal = Modal::None;
                }
                KeyCode::Down | KeyCode::Tab | KeyCode::Enter if count > 0 => {
                    self.session_search.focus_list();
                    self.clamp_picker_selection();
                }
                KeyCode::Backspace => {
                    self.session_search.input_mut().backspace();
                    self.session_search_changed();
                }
                KeyCode::Delete => {
                    self.session_search.input_mut().delete();
                    self.session_search_changed();
                }
                KeyCode::Left => self.session_search.input_mut().move_left(),
                KeyCode::Right => self.session_search.input_mut().move_right(),
                KeyCode::Home | KeyCode::Char('a')
                    if key.code == KeyCode::Home || key.modifiers == KeyModifiers::CONTROL =>
                {
                    self.session_search.input_mut().move_buffer_home();
                }
                KeyCode::End | KeyCode::Char('e')
                    if key.code == KeyCode::End || key.modifiers == KeyModifiers::CONTROL =>
                {
                    self.session_search.input_mut().move_buffer_end();
                }
                KeyCode::Char('u') if key.modifiers == KeyModifiers::CONTROL => {
                    self.session_search.input_mut().set_buffer(String::new());
                    self.session_search_changed();
                }
                KeyCode::Char(character) if is_printable_key(key) => {
                    self.session_search.input_mut().insert(character);
                    self.session_search_changed();
                }
                _ => {}
            }
            return;
        }
        match key.code {
            KeyCode::Esc | KeyCode::BackTab => self.session_search.focus_input(),
            KeyCode::Up if self.picker_state.selected().unwrap_or(0) == 0 => {
                self.session_search.focus_input();
            }
            KeyCode::Up => move_picker_selection(&mut self.picker_state, count, true),
            KeyCode::Down | KeyCode::Tab => {
                move_picker_selection(&mut self.picker_state, count, false)
            }
            KeyCode::Enter => {
                self.choose_picker_entry(self.picker_state.selected().unwrap_or(0))
                    .await
            }
            KeyCode::Char('u') if key.modifiers == KeyModifiers::CONTROL => {
                self.session_search.reset();
                self.session_search_changed();
            }
            KeyCode::Char(character) if is_printable_key(key) => {
                self.session_search.focus_input();
                self.session_search.input_mut().insert(character);
                self.session_search_changed();
            }
            _ => {}
        }
    }

    pub(in crate::ui) async fn handle_selection_picker(&mut self, key: KeyEvent) {
        let count = self.picker_entry_count();
        match key.code {
            KeyCode::Esc => {
                self.modal = Modal::None;
                self.new_session_draft = None;
            }
            KeyCode::Up => move_picker_selection(&mut self.picker_state, count, true),
            KeyCode::Down => move_picker_selection(&mut self.picker_state, count, false),
            KeyCode::Tab | KeyCode::BackTab => {
                cycle_selection(
                    &mut self.picker_state,
                    count,
                    agent_cycle_backward(key).unwrap_or(false),
                );
            }
            KeyCode::Enter => {
                self.choose_picker_entry(self.picker_state.selected().unwrap_or(0))
                    .await
            }
            _ => {}
        }
    }

    /// Close the model flow after a completed choice. Unlike the picker's
    /// Esc, this keeps any pending new-session draft the choice just edited.
    fn finish_model_choice(&mut self) {
        self.model_search.reset();
        self.model_then_variant = false;
        self.variant_step_model = None;
        self.modal = Modal::None;
    }

    /// Variants offered by `/model`'s variant step.
    pub(super) fn variant_step_options(&self) -> Vec<Option<VariantId>> {
        self.variant_step_model
            .as_ref()
            .map(|model| self.variants_for(model))
            .unwrap_or_default()
    }

    /// The variant to highlight first: the draft's own when it already uses
    /// `model`, otherwise the model's default.
    fn preferred_variant(&self, model: &ModelKey) -> Option<VariantId> {
        let draft = self.new_session_draft.as_ref().or(self.draft.as_ref());
        match draft {
            Some(draft) if &draft.model.model == model => draft.model.variant.clone(),
            _ => self
                .model_descriptor(model)
                .and_then(|descriptor| descriptor.default_variant.clone()),
        }
    }

    pub(super) fn variant_step_labels(&self) -> Vec<String> {
        let descriptor = self
            .variant_step_model
            .as_ref()
            .and_then(|model| self.model_descriptor(model));
        let default = descriptor.and_then(|descriptor| descriptor.default_variant.as_ref());
        self.variant_step_options()
            .iter()
            .map(|variant| {
                let name = match variant {
                    None => "base".to_owned(),
                    Some(id) => descriptor
                        .and_then(|descriptor| {
                            descriptor
                                .variants
                                .iter()
                                .find(|candidate| &candidate.id == id)
                        })
                        .map_or_else(
                            || id.to_string(),
                            |candidate| format!("{id} — {}", candidate.display_name),
                        ),
                };
                if variant.as_ref() == default {
                    format!("{name} (default)")
                } else {
                    name
                }
            })
            .collect()
    }

    /// Esc returns to the model list with its search intact; nothing was
    /// applied yet.
    pub(super) async fn handle_variant_picker_key(&mut self, key: KeyEvent) {
        let count = self.variant_step_options().len();
        match key.code {
            KeyCode::Esc => {
                let model = self.variant_step_model.take();
                let row = model
                    .and_then(|model| {
                        self.filtered_draft_models()
                            .iter()
                            .position(|selection| selection.model == model)
                    })
                    .unwrap_or(0);
                self.picker_state.select(Some(row));
                self.model_search.focus_list();
                self.modal = Modal::Models;
            }
            KeyCode::Up => move_picker_selection(&mut self.picker_state, count, true),
            KeyCode::Down => move_picker_selection(&mut self.picker_state, count, false),
            KeyCode::Enter => {
                self.choose_picker_entry(self.picker_state.selected().unwrap_or(0))
                    .await
            }
            _ => {}
        }
    }

    pub(super) async fn handle_agent_picker_key(&mut self, key: KeyEvent) {
        let count = self.filtered_agent_picker_candidates().len();
        if self.agent_search.focus() == SearchPickerFocus::Input {
            match key.code {
                KeyCode::Esc => self.close_agent_picker(),
                KeyCode::Down | KeyCode::Tab | KeyCode::Enter if count > 0 => {
                    self.agent_search.focus_list();
                    self.clamp_picker_selection();
                }
                KeyCode::Backspace => {
                    self.agent_search.input_mut().backspace();
                    self.agent_search_changed();
                }
                KeyCode::Delete => {
                    self.agent_search.input_mut().delete();
                    self.agent_search_changed();
                }
                KeyCode::Left => self.agent_search.input_mut().move_left(),
                KeyCode::Right => self.agent_search.input_mut().move_right(),
                KeyCode::Home | KeyCode::Char('a')
                    if key.code == KeyCode::Home || key.modifiers == KeyModifiers::CONTROL =>
                {
                    self.agent_search.input_mut().move_buffer_home();
                }
                KeyCode::End | KeyCode::Char('e')
                    if key.code == KeyCode::End || key.modifiers == KeyModifiers::CONTROL =>
                {
                    self.agent_search.input_mut().move_buffer_end();
                }
                KeyCode::Char('u') if key.modifiers == KeyModifiers::CONTROL => {
                    self.agent_search.input_mut().set_buffer(String::new());
                    self.agent_search_changed();
                }
                KeyCode::Char(character) if is_printable_key(key) => {
                    self.agent_search.input_mut().insert(character);
                    self.agent_search_changed();
                }
                _ => {}
            }
            return;
        }
        match key.code {
            KeyCode::Esc | KeyCode::BackTab => self.agent_search.focus_input(),
            KeyCode::Up if self.picker_state.selected().unwrap_or(0) == 0 => {
                self.agent_search.focus_input();
            }
            KeyCode::Up => move_picker_selection(&mut self.picker_state, count, true),
            KeyCode::Down | KeyCode::Tab => {
                move_picker_selection(&mut self.picker_state, count, false)
            }
            KeyCode::Enter => {
                self.choose_picker_entry(self.picker_state.selected().unwrap_or(0))
                    .await
            }
            KeyCode::Char('u') if key.modifiers == KeyModifiers::CONTROL => {
                self.agent_search.reset();
                self.agent_search_changed();
            }
            KeyCode::Char(character) if is_printable_key(key) => {
                self.agent_search.focus_input();
                self.agent_search.input_mut().insert(character);
                self.agent_search_changed();
            }
            _ => {}
        }
    }

    pub(super) async fn handle_model_picker_key(&mut self, key: KeyEvent) {
        let count = self.filtered_draft_models().len();
        if self.model_search.focus() == SearchPickerFocus::Input {
            match key.code {
                KeyCode::Esc => self.close_model_picker(),
                KeyCode::Down | KeyCode::Tab | KeyCode::Enter if count > 0 => {
                    self.model_search.focus_list();
                    self.clamp_picker_selection();
                }
                KeyCode::Backspace => {
                    self.model_search.input_mut().backspace();
                    self.model_search_changed();
                }
                KeyCode::Delete => {
                    self.model_search.input_mut().delete();
                    self.model_search_changed();
                }
                KeyCode::Left => self.model_search.input_mut().move_left(),
                KeyCode::Right => self.model_search.input_mut().move_right(),
                KeyCode::Home | KeyCode::Char('a')
                    if key.code == KeyCode::Home || key.modifiers == KeyModifiers::CONTROL =>
                {
                    self.model_search.input_mut().move_buffer_home();
                }
                KeyCode::End | KeyCode::Char('e')
                    if key.code == KeyCode::End || key.modifiers == KeyModifiers::CONTROL =>
                {
                    self.model_search.input_mut().move_buffer_end();
                }
                KeyCode::Char('u') if key.modifiers == KeyModifiers::CONTROL => {
                    self.model_search.input_mut().set_buffer(String::new());
                    self.model_search_changed();
                }
                KeyCode::Char(character) if is_printable_key(key) => {
                    self.model_search.input_mut().insert(character);
                    self.model_search_changed();
                }
                _ => {}
            }
            return;
        }
        match key.code {
            KeyCode::Esc | KeyCode::BackTab => self.model_search.focus_input(),
            KeyCode::Up if self.picker_state.selected().unwrap_or(0) == 0 => {
                self.model_search.focus_input();
            }
            KeyCode::Up => move_picker_selection(&mut self.picker_state, count, true),
            KeyCode::Down | KeyCode::Tab => {
                move_picker_selection(&mut self.picker_state, count, false)
            }
            KeyCode::Enter => {
                self.choose_picker_entry(self.picker_state.selected().unwrap_or(0))
                    .await
            }
            KeyCode::Char('u') if key.modifiers == KeyModifiers::CONTROL => {
                self.model_search.reset();
                self.model_search_changed();
            }
            KeyCode::Char(character) if is_printable_key(key) => {
                self.model_search.focus_input();
                self.model_search.input_mut().insert(character);
                self.model_search_changed();
            }
            _ => {}
        }
    }

    pub(in crate::ui) async fn choose_picker_entry(&mut self, index: usize) {
        match self.modal {
            Modal::Sessions => {
                if let Some(session_id) = self.session_search_ids().get(index).copied() {
                    self.modal = Modal::None;
                    self.session_search.reset();
                    self.open_session(session_id).await;
                }
            }
            Modal::Agents => {
                if self.new_session_draft.is_none() && !self.agent_switching_allowed() {
                    self.status = self
                        .delegated_pin_reason()
                        .unwrap_or_else(|| "agent switching requires a root session".into());
                    return;
                }
                let agent = self
                    .filtered_agent_picker_candidates()
                    .get(index)
                    .map(|agent| agent.id.clone());
                if let Some(agent) = agent {
                    self.set_draft_agent(agent);
                    self.agent_search.reset();
                    self.modal = Modal::None;
                    // Agent selection only updates the client-side draft.
                    // Creation is deferred until the first prompt so the
                    // session and its first run follow one admission flow.
                }
            }
            Modal::Presets => {
                let preset = if index == 0 {
                    Some(None)
                } else {
                    self.preset_names().get(index - 1).cloned().map(Some)
                };
                if let Some(preset) = preset {
                    let preferred_agent = self
                        .new_session_draft
                        .as_ref()
                        .or(self.draft.as_ref())
                        .map(|draft| draft.agent.clone());
                    if self.new_session_draft.is_none() {
                        self.set_draft_reset_intent(true);
                    }
                    self.selected_preset = preset;
                    if self.new_session_draft.is_some() {
                        self.new_session_draft = self.draft_selection_for_preset(
                            self.selected_preset.as_deref(),
                            preferred_agent.as_ref(),
                        );
                    } else if self.watching_root_session() {
                        self.draft = self.draft_selection_for_preset(
                            self.selected_preset.as_deref(),
                            preferred_agent.as_ref(),
                        );
                    }
                    if self.draft.is_none() && self.watching_root_session() {
                        self.open_selection_modal(Modal::Agents);
                    } else {
                        self.modal = Modal::None;
                    }
                    self.status = if self.watching_root_session() {
                        self.draft_status("Draft run preset")
                    } else {
                        format!(
                            "Agent preset for the next root run and future new sessions: {}; delegated session remains pinned",
                            self.selected_preset_label()
                        )
                    };
                }
            }
            Modal::Models => {
                if !self.model_selection_allowed() {
                    self.status = "no draft model is available for this session".into();
                    return;
                }
                let model = self
                    .filtered_draft_models()
                    .get(index)
                    .map(|selection| selection.model.clone());
                let Some(model) = model else {
                    return;
                };
                if self.model_then_variant && self.variants_for(&model).len() > 1 {
                    // Nothing is applied until the variant is chosen, so Esc
                    // from the variant step leaves the draft untouched.
                    let preferred = self.preferred_variant(&model);
                    let row = self
                        .variants_for(&model)
                        .iter()
                        .position(|variant| *variant == preferred)
                        .unwrap_or(0);
                    self.variant_step_model = Some(model);
                    self.picker_state.select(Some(row));
                    self.modal = Modal::Variants;
                    return;
                }
                self.set_draft_model(model);
                self.finish_model_choice();
            }
            Modal::Variants => {
                let Some(model) = self.variant_step_model.clone() else {
                    return;
                };
                let Some(variant) = self.variant_step_options().get(index).cloned() else {
                    return;
                };
                self.set_draft_model(model);
                self.set_draft_variant(variant);
                self.finish_model_choice();
            }
            Modal::ConnectProviders => {
                self.picker_state.select(Some(index));
                self.provider_search.focus_list();
                self.handle_connect_provider_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
            }
            Modal::ConnectDetails
            | Modal::ConnectSetup
            | Modal::ConnectError
            | Modal::DisconnectConfirm => {}
            Modal::UserMessage => self.activate_user_menu_entry(index),
            Modal::RevertConfirm => {}
            Modal::Mcp | Modal::Permissions | Modal::Usage | Modal::GoalDetail => {}
            Modal::None => {}
        }
    }
}
