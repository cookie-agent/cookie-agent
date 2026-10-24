//! A session's ACP agent in the TUI (`docs/site/specs/acp-agents.md`
//! sections 5 and 10): the agent's slash commands in the command palette,
//! marked as the agent's, and its config options in `/options`.

use super::*;

use cookie_agent_client_state::AcpSessionView;
use cookie_agent_protocol::{AcpCommand, AcpConfigOption, AcpConfigOptionKind};

/// A palette row for one of the agent's commands.
pub(super) fn agent_command_label(command: &AcpCommand) -> String {
    let hint = command
        .input_hint
        .as_deref()
        .map_or(String::new(), |hint| format!(" {hint}"));
    if command.description.trim().is_empty() {
        format!("/{}{hint} — (agent)", command.name)
    } else {
        format!("/{}{hint} — (agent) {}", command.name, command.description)
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

    /// Chooses agent command `command`: one that takes input asks for it,
    /// one that takes none is sent at once.
    pub(super) async fn choose_agent_command(&mut self, command: AcpCommand) {
        if command.input_hint.is_some() {
            self.push_palette_step(PaletteStep::Text {
                target: TextTarget::AgentCommand {
                    name: command.name,
                    hint: command.input_hint,
                },
                input: InputState::default(),
            });
            return;
        }
        self.close_command_palette();
        self.submit_prompt(agent_command_prompt(&command.name, ""))
            .await;
    }

    /// Opens `/options` on the session's agent options.
    pub(super) fn open_agent_options(&mut self) {
        match self.viewed_acp() {
            None => {
                self.status =
                    "the agent's options appear once its session has started: send a first message"
                        .into();
                return;
            }
            Some(acp) if acp.options.is_empty() => {
                self.status = "the session's agent offers no options".into();
                return;
            }
            Some(_) => {}
        }
        self.agent_option_step = None;
        self.picker_state.select(Some(0));
        self.modal = Modal::AgentOptions;
    }

    fn agent_options(&self) -> &[AcpConfigOption] {
        self.viewed_acp().map_or(&[], |acp| acp.options.as_slice())
    }

    fn agent_option_in_step(&self) -> Option<&AcpConfigOption> {
        let id = self.agent_option_step.as_deref()?;
        self.agent_options().iter().find(|option| option.id == id)
    }

    /// The title and rows of `/options`: the options, or the values of the
    /// option being changed, the current one marked.
    pub(super) fn agent_option_rows(&self) -> (String, Vec<String>) {
        let agent = self
            .viewed_acp()
            .and_then(|acp| acp.agent.as_ref())
            .map_or_else(String::new, ToString::to_string);
        match self.agent_option_in_step() {
            Some(option) => (
                format!("{} — {agent}", option.name),
                option
                    .values
                    .iter()
                    .map(|value| {
                        if value.value == option.current_value {
                            format!("{} (current)", value.name)
                        } else {
                            value.name.clone()
                        }
                    })
                    .collect(),
            ),
            None => (
                format!("Agent options — {agent}"),
                self.agent_options().iter().map(option_label).collect(),
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
    /// one toggles; a value sets it on the agent.
    pub(super) fn choose_agent_option(&mut self, index: usize) {
        if let Some(option) = self.agent_option_in_step() {
            let Some(value) = option.values.get(index) else {
                return;
            };
            let (option_id, value_id) = (option.id.clone(), value.value.clone());
            let label = format!("{}: {}", option.name, value.name);
            self.agent_option_step = None;
            self.modal = Modal::None;
            self.set_agent_option(option_id, value_id, label);
            return;
        }
        let Some(option) = self.agent_options().get(index) else {
            return;
        };
        // A boolean toggles at once; the picker stays open on it.
        if option.kind == AcpConfigOptionKind::Boolean {
            let on = option.current_value != "true";
            let label = format!("{}: {}", option.name, if on { "on" } else { "off" });
            let option_id = option.id.clone();
            self.set_agent_option(option_id, on.to_string(), label);
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
