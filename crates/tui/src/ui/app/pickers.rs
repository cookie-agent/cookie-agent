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

impl App {
    pub(in crate::ui) fn command_palette_visible(&self) -> bool {
        let command = self.input.as_str().strip_prefix('/').unwrap_or_default();
        self.modal == Modal::None
            && self.current_approval().is_none()
            && self.input_focused
            && self.input.as_str().starts_with('/')
            && !self.input.as_str().starts_with("//")
            && !command.chars().any(char::is_whitespace)
            && !self.palette_dismissed
    }

    pub(in crate::ui) fn palette_entries(&self) -> Vec<PaletteEntry<'_>> {
        let query = self
            .input
            .as_str()
            .strip_prefix('/')
            .unwrap_or_default()
            .split_whitespace()
            .next()
            .unwrap_or_default()
            .to_ascii_lowercase();
        crate::ui::slash::entries(self.input.as_str())
            .into_iter()
            .filter(|spec| self.command_is_available(spec))
            .map(PaletteEntry::Command)
            .chain(
                self.skills
                    .iter()
                    .filter(|skill| {
                        skill.precedence_winner
                            && skill.user_invocable
                            && (query.is_empty() || skill.name.contains(&query))
                    })
                    .map(PaletteEntry::Skill),
            )
            .collect()
    }

    pub(in crate::ui) async fn handle_palette_key(&mut self, key: KeyEvent) {
        if is_newline_key(key) {
            self.mutate_input(|input| input.insert_newline());
            self.clamp_palette_selection();
            return;
        }
        let entry_count = self.palette_entries().len();
        match key.code {
            KeyCode::Esc => {
                self.palette_dismissed = true;
                self.last_escape = None;
            }
            KeyCode::Up => move_selection(&mut self.palette_state, entry_count, true),
            KeyCode::Down => move_selection(&mut self.palette_state, entry_count, false),
            KeyCode::Enter if self.palette_entries().is_empty() => self.submit_input().await,
            KeyCode::Enter => {
                self.activate_palette_entry(self.palette_state.selected().unwrap_or(0))
                    .await
            }
            _ => {
                self.handle_input_key(key).await;
                self.clamp_palette_selection();
            }
        }
    }

    pub(in crate::ui) fn clamp_palette_selection(&mut self) {
        let entries = self.palette_entries();
        self.palette_state.select((!entries.is_empty()).then(|| {
            self.palette_state
                .selected()
                .unwrap_or(0)
                .min(entries.len() - 1)
        }));
    }

    pub(in crate::ui) async fn activate_palette_entry(&mut self, index: usize) {
        let Some(entry) = self.palette_entries().get(index).copied() else {
            return;
        };
        let (usage, requires_arguments) = match entry {
            PaletteEntry::Command(spec) => (spec.usage.to_owned(), spec.requires_arguments),
            PaletteEntry::Skill(skill) => (format!("/{}", skill.name), true),
        };
        self.palette_dismissed = true;
        self.palette_state.select(Some(0));
        self.mutate_input(|input| {
            input.set_buffer(if requires_arguments {
                format!(
                    "{} ",
                    usage.split_whitespace().next().expect("command usage")
                )
            } else {
                usage
            });
        });
        if !requires_arguments {
            self.submit_input().await;
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
                if let Some(model) = model {
                    self.set_draft_model(model);
                    self.model_search.reset();
                    self.modal = Modal::None;
                }
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
            Modal::Mcp | Modal::Permissions | Modal::Skills | Modal::Usage | Modal::GoalDetail => {}
            Modal::None => {}
        }
    }
}
