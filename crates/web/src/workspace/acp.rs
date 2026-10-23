//! A session's ACP agent in the web client
//! (`docs/site/specs/acp-agents.md` sections 5 and 10): its commands in the
//! composer's suggestions, and its options in the agent options sheet the
//! model chip opens.

use cookie_agent_client_state::AcpSessionView;
use cookie_agent_protocol::{AcpCommand, AgentDescriptor};
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

    /// The agent's commands a composer draft starting with `/` matches:
    /// its first word, without the `/`, is a prefix of the command's name.
    /// Nothing once the draft has more than the command's name.
    pub fn agent_command_suggestions(self, draft: &str) -> Vec<AcpCommand> {
        let Some(name) = draft.strip_prefix('/') else {
            return Vec::new();
        };
        if name.contains(char::is_whitespace) {
            return Vec::new();
        }
        self.acp()
            .map(|acp| acp.commands_matching(name).cloned().collect())
            .unwrap_or_default()
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

    /// The model chip's label for an ACP agent: the value of its `model`
    /// option when it has one.
    pub fn acp_model_label(self) -> Option<String> {
        let acp = self.acp()?;
        let option = acp
            .options
            .iter()
            .find(|option| option.category.as_deref() == Some("model") || option.id == "model")?;
        Some(
            option
                .values
                .iter()
                .find(|value| value.value == option.current_value)
                .map_or_else(|| option.current_value.clone(), |value| value.name.clone()),
        )
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
