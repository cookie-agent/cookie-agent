//! A session's ACP agent in the web client
//! (`docs/site/specs/acp-agents.md` sections 5 and 10): its commands in the
//! session commands, its model and effort options in the model chip
//! and sheet, and its other options in the agent options sheet.

use cookie_agent_client_state::AcpSessionView;
use cookie_agent_protocol::{AcpCommand, AcpConfigOption, AgentDescriptor};
use leptos::{prelude::*, task::spawn_local};

use super::Workspace;

impl Workspace {
    /// What the viewed session's ACP agent reported, when it has one.
    pub fn acp(self) -> Option<AcpSessionView> {
        if self.new_session.get() {
            return None;
        }
        let session_id = self.selected.get()?;
        self.with_store(|store| {
            store
                .sessions
                .get(&session_id)
                .map(|state| state.acp.clone())
                .filter(|acp| acp.agent.is_some())
        })
    }

    /// Whether the draft runs an ACP agent, which chooses its own model.
    pub fn draft_is_acp(self) -> bool {
        self.draft
            .get()
            .is_some_and(|draft| cookie_agent_client_state::acp::is_acp_model(&draft.model))
    }

    /// The viewed session's agent's commands, listed in the session
    /// commands after cookie's own. None where the agent takes no prompts
    /// (its own subagent's session).
    pub fn agent_commands(self) -> Vec<AcpCommand> {
        self.acp()
            .filter(|acp| acp.refusal().is_none())
            .map(|acp| acp.commands)
            .unwrap_or_default()
    }

    /// Whether the agent's own command stands in for cookie's `name`.
    pub fn agent_replaces(self, name: &str) -> bool {
        self.acp().is_some_and(|acp| acp.replaces(name))
    }

    /// Chooses agent command `command`: a sheet asks for its optional
    /// input. The agent's `compact` is cookie's compaction, which runs it.
    pub fn choose_agent_command(self, command: AcpCommand) {
        if command.name == "compact" {
            self.run_command(crate::commands::Command::Compact);
            return;
        }
        self.agent_command.set(Some(command));
        self.panel.set(Some(super::Panel::AgentCommand));
    }

    /// Sends agent command `name` with `input` to the viewed session, as
    /// ACP specifies: the prompt `/<name> <input>`, or a skill's
    /// `$<skill> <input>`.
    pub fn run_agent_command(self, name: &str, input: &str) {
        let text = cookie_agent_client_state::acp::command_prompt(name, input);
        if !self.writable() {
            self.status
                .set("This session is owned by another cookie process; input is disabled.".into());
            return;
        }
        if let Some(refusal) = self.acp().and_then(|acp| acp.refusal()) {
            self.status.set(refusal);
            return;
        }
        self.panel.set(None);
        self.send_content(
            cookie_agent_protocol::UserContent::text(text.clone()),
            cookie_agent_protocol::InputDelivery::Steer,
            text,
            Vec::new(),
        );
    }

    /// The agents the agent picker lists: the root agents of the preset in
    /// use, and ACP agents that are unavailable now, shown dimmed.
    pub fn picker_agents(self) -> Vec<AgentDescriptor> {
        let preset = self.preset.get();
        self.with_runtime(|runtime| {
            runtime
                .snapshot()
                .map(|snapshot| {
                    snapshot
                        .agents
                        .iter()
                        .filter(|agent| {
                            cookie_agent_client_state::acp::listed_in_root_picker(
                                agent,
                                preset.as_deref(),
                            )
                        })
                        .cloned()
                        .collect()
                })
                .unwrap_or_default()
        })
    }

    /// The ACP agent the draft runs, when it runs one.
    pub(crate) fn draft_acp_agent(self) -> Option<AgentDescriptor> {
        let draft = self.draft.get()?;
        if !cookie_agent_client_state::acp::is_acp_model(&draft.model) {
            return None;
        }
        self.with_runtime(|runtime| {
            runtime.snapshot().and_then(|snapshot| {
                snapshot
                    .agents
                    .iter()
                    .find(|agent| agent.id == draft.agent && agent.preset == draft.preset)
                    .cloned()
            })
        })
    }

    /// The options of the draft's ACP agent
    /// (`docs/site/specs/acp-agents.md` section 15, input and pickers): a
    /// new session's draft offers what the agent last started a session
    /// with on the runtime it would run on, with the draft's choices; a
    /// session's are its agent's, or, before its agent session started,
    /// what it was created with.
    pub fn acp_options(self) -> Vec<AcpConfigOption> {
        let attached = self.agent_runtimes();
        let agent = self.draft_acp_agent();
        if self.new_session.get() {
            return agent.map_or_else(Vec::new, |agent| {
                self.draft_agent_options.with(|draft| {
                    cookie_agent_client_state::acp::draft_options(
                        &agent,
                        &attached,
                        draft.for_agent(&agent.id),
                    )
                })
            });
        }
        let Some(session_id) = self.selected.get() else {
            return Vec::new();
        };
        self.with_store(|store| {
            store
                .sessions
                .get(&session_id)
                .map(|state| {
                    cookie_agent_client_state::acp::session_options(
                        &state.acp,
                        agent.as_ref(),
                        &attached,
                    )
                })
                .unwrap_or_default()
        })
    }

    /// The model chip's label for an ACP agent: its model option's value
    /// and its effort option's, as cookie labels a model and its variant.
    pub fn acp_model_label(self) -> Option<String> {
        cookie_agent_client_state::acp::model_label(&self.acp_options())
    }

    /// Chooses `value` for the agent's option `option_id`: a new session's
    /// draft keeps it for `session.create`; a session sets it on its agent.
    pub fn choose_agent_option(self, option_id: String, value: String, label: String) {
        if self.new_session.get_untracked() {
            let Some(agent) = self.draft.get_untracked().map(|draft| draft.agent) else {
                return;
            };
            self.draft_agent_options
                .update(|draft| draft.choose(&agent, &option_id, &value));
            self.status.set(format!("draft agent option {label}"));
            return;
        }
        self.set_agent_option(option_id, value, label);
    }

    /// Sets the viewed session's agent option `option_id` to `value`
    /// (`session.agent_option.set`).
    pub fn set_agent_option(self, option_id: String, value: String, label: String) {
        let (Some(client), Some(session_id)) = (self.client(), self.selected.get_untracked())
        else {
            return;
        };
        self.status.set(format!("setting {label}…"));
        spawn_local(async move {
            match client
                .set_session_agent_option(session_id, option_id, value)
                .await
            {
                Ok(result) => {
                    self.update_store(|store| {
                        if let Some(state) = store.sessions.get_mut(&session_id) {
                            state.acp.options = result.options;
                        }
                    });
                    self.status.set(format!("agent option {label}"));
                }
                Err(error) => self.status.set(format!(
                    "setting {label} failed: {}",
                    crate::errors::describe_full(&error)
                )),
            }
        });
    }
}
