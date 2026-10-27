//! A session's ACP agent in the TUI (`docs/site/specs/acp-agents.md`
//! sections 5 and 10): the agent's slash commands in the command palette,
//! marked as the agent's, and its config options in `/options`, its model
//! and effort as the model and variant (section 15, input and pickers).

use super::*;

use cookie_agent_client_state::AcpSessionView;
use cookie_agent_protocol::{AcpCommand, AcpConfigOption, AcpConfigOptionKind};

/// A palette row for one of the agent's commands, drawn after its "agent"
/// badge.
pub(super) fn agent_command_label(command: &AcpCommand) -> String {
    let hint = command
        .input_hint
        .as_deref()
        .map_or(String::new(), |hint| format!(" {hint}"));
    if command.description.trim().is_empty() {
        format!("/{}{hint}", command.name)
    } else {
        format!("/{}{hint} — {}", command.name, command.description)
    }
}

/// The prompt that runs agent command `name` with `input`, as ACP
/// specifies: `/<name> <input>`.
pub(super) fn agent_command_prompt(name: &str, input: &str) -> String {
    let input = input.trim();
    if input.is_empty() {
        format!("/{name}")
    } else {
        format!("/{name} {input}")
    }
}

#[cfg(test)]
pub(crate) fn agent_command_prompt_for_test(name: &str, input: &str) -> String {
    agent_command_prompt(name, input)
}

/// A row of `/options`: the option and its current value's name (`On` or
/// `Off` for a boolean).
fn option_label(option: &AcpConfigOption) -> String {
    let current = option
        .values
        .iter()
        .find(|value| value.value == option.current_value)
        .map_or(option.current_value.as_str(), |value| value.name.as_str());
    match option.category.as_deref() {
        Some(category) if category != option.name.to_lowercase() => {
            format!("{}: {current} ({category})", option.name)
        }
        _ => format!("{}: {current}", option.name),
    }
}

impl App {
    /// What the ACP agent of the session on screen reported, when it has
    /// one.
    pub(in crate::ui) fn viewed_acp(&self) -> Option<&AcpSessionView> {
        self.viewed_session()
            .and_then(|session_id| self.store.sessions.get(&session_id))
            .map(|state| &state.acp)
            .filter(|acp| acp.agent.is_some())
    }

    /// The agent's commands matching the palette's search, listed after
    /// cookie's own.
    pub(in crate::ui) fn palette_agent_commands(&self) -> Vec<AcpCommand> {
        let Some(palette) = &self.palette else {
            return Vec::new();
        };
        if !palette.steps.is_empty() || !self.session_writable() {
            return Vec::new();
        }
        let query = crate::ui::slash::normalized_query(palette.search.as_str());
        let query = query.trim_start_matches('/');
        self.viewed_acp()
            .map(|acp| {
                acp.commands
                    .iter()
                    .filter(|command| {
                        crate::ui::slash::match_rank([command.name.as_str()], query).is_some()
                    })
                    .cloned()
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Chooses agent command `command`: a box asks for its input, which may
    /// be left empty.
    pub(super) fn choose_agent_command(&mut self, command: AcpCommand) {
        self.push_palette_step(PaletteStep::Text {
            target: TextTarget::AgentCommand {
                name: command.name,
                hint: command.input_hint,
            },
            input: InputState::default(),
        });
    }

    /// Runs agent command `name` with `input`: the agent's `compact` as
    /// cookie's compaction, which waits for it, any other as a prompt.
    pub(super) async fn run_agent_command(&mut self, name: &str, input: &str) {
        let input = input.trim();
        if name == "compact" {
            self.compact_selected_session((!input.is_empty()).then(|| input.to_owned()))
                .await;
        } else {
            self.submit_prompt(agent_command_prompt(name, input)).await;
        }
    }

    /// Whether the session's agent offers its own `name`, which stands in
    /// for cookie's.
    pub(in crate::ui) fn agent_replaces(&self, name: &str) -> bool {
        self.viewed_acp().is_some_and(|acp| acp.replaces(name))
    }

    /// The ACP agent the draft runs, when it runs one.
    pub(super) fn draft_acp_agent(&self) -> Option<&cookie_agent_protocol::AgentDescriptor> {
        let draft = self.new_session_draft.as_ref().or(self.draft.as_ref())?;
        if !cookie_agent_client_state::acp::is_acp_model(&draft.model) {
            return None;
        }
        self.agents
            .iter()
            .find(|agent| agent.id == draft.agent && agent.preset == draft.preset)
    }

    /// The options of the draft's ACP agent (`docs/site/specs/acp-agents.md`
    /// section 15, input and pickers): a new session's draft offers what the
    /// agent last started a session with on the runtime it would run on,
    /// with the draft's choices; a session's are its agent's, or, before its
    /// agent session started, what it was created with.
    pub(in crate::ui) fn agent_options(&self) -> Vec<AcpConfigOption> {
        let attached = self.agent_runtimes();
        let agent = self.draft_acp_agent();
        if self.new_session_draft.is_some() {
            return agent.map_or_else(Vec::new, |agent| {
                cookie_agent_client_state::acp::draft_options(
                    agent,
                    &attached,
                    self.draft_agent_options.for_agent(&agent.id),
                )
            });
        }
        let Some(state) = self
            .viewed_session()
            .and_then(|session_id| self.store.sessions.get(&session_id))
        else {
            return Vec::new();
        };
        cookie_agent_client_state::acp::session_options(&state.acp, agent, &attached)
    }

    /// The draft's ACP model and effort for the title, when the agent
    /// offers a model option.
    pub(in crate::ui) fn acp_model_and_effort(&self) -> Option<(String, Option<String>)> {
        cookie_agent_client_state::acp::model_and_effort(&self.agent_options())
    }

    /// Opens `/options` on the session's agent options.
    pub(super) fn open_agent_options(&mut self) {
        if self.agent_options().is_empty() {
            self.status = if self.viewed_acp().is_some() {
                "the session's agent offers no options".into()
            } else {
                "the agent's options appear once it has started a session: send a first message"
                    .into()
            };
            return;
        }
        self.agent_option_step = None;
        self.picker_state.select(Some(0));
        self.modal = Modal::AgentOptions;
    }

    /// Opens the values of the agent's model option (`model`) or effort
    /// option (`effort`) as the model or variant picker, else `/options`.
    pub(super) fn open_agent_model_picker(&mut self, effort: bool) {
        let options = self.agent_options();
        let option = if effort {
            cookie_agent_client_state::acp::effort_option(&options)
        } else {
            cookie_agent_client_state::acp::model_option(&options)
        };
        let Some(option) = option.filter(|option| !option.values.is_empty()) else {
            self.open_agent_options();
            return;
        };
        let current = option
            .values
            .iter()
            .position(|value| value.value == option.current_value)
            .unwrap_or(0);
        self.agent_option_step = Some(option.id.clone());
        self.picker_state.select(Some(current));
        self.modal = Modal::AgentOptions;
    }

    /// The next value of the agent's effort option, as the variant segment
    /// cycles cookie's own variants.
    pub(super) fn cycle_agent_effort(&mut self) {
        let options = self.agent_options();
        let Some(option) = cookie_agent_client_state::acp::effort_option(&options) else {
            self.status = "the agent offers no effort option".into();
            return;
        };
        if option.values.len() <= 1 {
            return;
        }
        let index = option
            .values
            .iter()
            .position(|value| value.value == option.current_value)
            .map_or(0, |index| (index + 1) % option.values.len());
        let value = &option.values[index];
        let label = format!("{}: {}", option.name, value.name);
        self.apply_agent_option(option.id.clone(), value.value.clone(), label);
    }

    fn agent_option_in_step(&self) -> Option<AcpConfigOption> {
        let id = self.agent_option_step.as_deref()?;
        self.agent_options()
            .into_iter()
            .find(|option| option.id == id)
    }

    /// The title and rows of `/options`, `width` columns wide: the
    /// options, or the values of the option being changed, the current one
    /// marked and each with its ID dimmed (what an agent document's
    /// `models` entry writes for a model or effort).
    pub(super) fn agent_option_rows(&self, width: usize) -> (String, Vec<Line<'static>>) {
        let agent = self
            .draft_acp_agent()
            .map(|agent| &agent.id)
            .or_else(|| self.viewed_acp().and_then(|acp| acp.agent.as_ref()))
            .map_or_else(String::new, ToString::to_string);
        let selected = self.picker_state.selected();
        match self.agent_option_in_step() {
            Some(option) => (
                format!("{} — {agent}", option.name),
                option
                    .values
                    .iter()
                    .enumerate()
                    .map(|(index, value)| {
                        let suffix = if value.value == option.current_value {
                            " (current)"
                        } else {
                            ""
                        };
                        super::pickers::document_id_row(
                            &value.name,
                            suffix,
                            &value.value,
                            width,
                            selected == Some(index),
                            &self.theme,
                        )
                    })
                    .collect(),
            ),
            None => (
                format!("Agent options — {agent}"),
                self.agent_options()
                    .iter()
                    .enumerate()
                    .map(|(index, option)| {
                        let style = if selected == Some(index) {
                            self.theme.body().patch(self.theme.selected())
                        } else {
                            self.theme.body()
                        };
                        Line::from(Span::styled(
                            truncate_with_ellipsis(&option_label(option), width),
                            style,
                        ))
                    })
                    .collect(),
            ),
        }
    }

    pub(super) fn agent_option_row_count(&self) -> usize {
        match self.agent_option_in_step() {
            Some(option) => option.values.len(),
            None => self.agent_options().len(),
        }
    }

    /// Row `index` of `/options`: an option opens its values, and a boolean
    /// one toggles; a value sets it on the agent, or on the draft.
    pub(super) fn choose_agent_option(&mut self, index: usize) {
        if let Some(option) = self.agent_option_in_step() {
            let Some(value) = option.values.get(index) else {
                return;
            };
            let (option_id, value_id) = (option.id.clone(), value.value.clone());
            let label = format!("{}: {}", option.name, value.name);
            self.agent_option_step = None;
            self.modal = Modal::None;
            self.apply_agent_option(option_id, value_id, label);
            return;
        }
        let Some(option) = self.agent_options().get(index).cloned() else {
            return;
        };
        // A boolean toggles at once; the picker stays open on it.
        if option.kind == AcpConfigOptionKind::Boolean {
            let on = option.current_value != "true";
            let label = format!("{}: {}", option.name, if on { "on" } else { "off" });
            let option_id = option.id.clone();
            self.apply_agent_option(option_id, on.to_string(), label);
            return;
        }
        if option.values.is_empty() {
            self.status = format!("the agent lists no values for {}", option.name);
            return;
        }
        let current = option
            .values
            .iter()
            .position(|value| value.value == option.current_value)
            .unwrap_or(0);
        self.agent_option_step = Some(option.id.clone());
        self.picker_state.select(Some(current));
    }

    /// Chooses `value` for option `option_id`: a new session's draft keeps
    /// it for `session.create`; a session sets it on its agent.
    fn apply_agent_option(&mut self, option_id: String, value: String, label: String) {
        if let Some(draft) = &self.new_session_draft {
            let agent = draft.agent.clone();
            self.draft_agent_options.choose(&agent, &option_id, &value);
            self.status = format!("Draft agent option {label}");
            return;
        }
        self.set_agent_option(option_id, value, label);
    }

    /// Sets the session's agent option `option_id` to `value`
    /// (`session.agent_option.set`).
    fn set_agent_option(&mut self, option_id: String, value: String, label: String) {
        let Some(session_id) = self.viewed_session() else {
            return;
        };
        self.status = format!("Setting {label}…");
        let client = self.client.clone();
        let updates = self.rpc_updates_tx.clone();
        self.spawn_rpc(async move {
            let result = client
                .set_session_agent_option(session_id, option_id, value)
                .await
                .map(|result| result.options)
                .map_err(|error| error.to_string());
            let _ = updates.send(RpcUpdate::AgentOptionSet {
                session_id,
                label,
                result,
            });
        });
    }

    pub(super) fn finish_agent_option_set(
        &mut self,
        session_id: SessionId,
        label: String,
        result: Result<Vec<AcpConfigOption>, String>,
    ) {
        match result {
            Ok(options) => {
                if let Some(state) = self.store.sessions.get_mut(&session_id) {
                    state.acp.options = options;
                    state.version = state.version.wrapping_add(1);
                }
                self.status = format!("Agent option {label}");
            }
            Err(error) => self.status = format!("setting {label} failed: {error}"),
        }
    }
}
