//! The Agents view's panel (`docs/site/specs/acp-onboarding.md` section 7):
//! `agents.documents`, and the changes the panel makes, each followed by a
//! fresh read of the documents and the configuration.

use cookie_agent_client_state::agent_documents::AgentDocumentAction;
use cookie_agent_protocol::{
    AgentDocumentParams, AgentsDocumentsParams, AgentsResolvedParams, AgentsResolvedResult,
    AgentsSetEnabledParams,
};
use leptos::{prelude::*, task::spawn_local};

use super::Workspace;
use crate::layout::{SettingsSection, View};

/// An agent the Agents view shows read-only: one with no document of the
/// user's (an official agent, an ACP agent that follows its template, an
/// official template), or an official document by its path
/// (`docs/site/specs/agent-shortcuts.md` section 3.1).
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct AgentOpen {
    /// The agent's ID, or a template's `_<name>`; empty for a document
    /// opened by `path`.
    pub id: String,
    pub preset: Option<String>,
    /// An official document's tree path, opened from a chain's pill.
    pub path: Option<String>,
    /// Where it comes from, as the panel says it (`official`, `follows
    /// template`).
    pub origin: String,
    /// The panel's actions on it: Customize, Turn off, ….
    pub info: Option<cookie_agent_protocol::AgentDocumentInfo>,
}

impl AgentOpen {
    /// What `agents.resolved` is asked for it.
    #[must_use]
    pub fn params(&self) -> AgentsResolvedParams {
        match &self.path {
            Some(path) => AgentsResolvedParams {
                path: Some(path.clone()),
                ..AgentsResolvedParams::default()
            },
            None => AgentsResolvedParams {
                id: Some(self.id.clone()),
                preset: self.preset.clone(),
                ..AgentsResolvedParams::default()
            },
        }
    }

    /// Its name, for the page's heading.
    #[must_use]
    pub fn name(&self) -> String {
        match &self.path {
            Some(path) => path
                .rsplit('/')
                .next()
                .unwrap_or(path)
                .trim_end_matches(".md")
                .to_owned(),
            None => self.id.clone(),
        }
    }
}

/// The tab an agent document of the user's shows.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum AgentTab {
    Form,
    File,
    /// The document after every `inherit`, read-only.
    #[default]
    Resolved,
}

impl Workspace {
    /// Opens the user's agent document at `path` on Resolved.
    pub fn show_agent_document(self, path: &str) {
        self.agent_tab.set(AgentTab::Resolved);
        self.edit_config(path);
        self.sidebar_open.set(false);
    }

    /// Shows `open` read-only, in place of any agent document in the
    /// editor.
    pub fn open_agent_read_only(self, open: AgentOpen) {
        if self.config_edit.with_untracked(|edit| {
            edit.as_ref()
                .is_some_and(|edit| edit.path.starts_with("agents/"))
        }) {
            self.config_edit.set(None);
        }
        self.config_error.set(None);
        self.agent_open.set(Some(open));
        self.sidebar_open.set(false);
    }

    /// Opens the document at tree path `path`: the user's in the editor,
    /// an official one read-only.
    pub fn open_agent_path(self, path: &str) {
        if path.starts_with("agents/") {
            self.show_agent_document(path);
        } else {
            self.open_agent_read_only(AgentOpen {
                path: Some(path.to_owned()),
                origin: "official".to_owned(),
                ..AgentOpen::default()
            });
        }
    }

    /// The composer's Edit agents: the Agents view with nothing open
    /// (`docs/site/specs/agent-shortcuts.md` decision 1).
    pub fn edit_agents(self) {
        if self.config_edit.with_untracked(|edit| {
            edit.as_ref()
                .is_some_and(|edit| edit.path.starts_with("agents/"))
        }) {
            self.config_edit.set(None);
        }
        self.agent_open.set(None);
        self.open_view(View::Agents, None);
    }

    /// The model menu's Edit model providers: Settings › Providers.
    pub fn edit_providers(self) {
        self.open_view(View::Settings, Some(SettingsSection::Providers));
    }

    /// Reads `agents.resolved` for `params` into `into`, with what it
    /// asked, so a view that moved on can tell.
    pub fn read_resolved_agent(
        self,
        params: AgentsResolvedParams,
        into: RwSignal<Option<(AgentsResolvedParams, Result<AgentsResolvedResult, String>)>>,
    ) {
        let Some(client) = self.client() else {
            return;
        };
        spawn_local(async move {
            let result = client
                .resolved_agent(params.clone())
                .await
                .map_err(|error| crate::errors::describe(&error));
            into.set(Some((params, result)));
        });
    }

    /// Reads `agents.documents`.
    pub fn poll_agent_documents(self) {
        let Some(client) = self.client() else {
            return;
        };
        spawn_local(async move {
            let result = client
                .agent_documents(AgentsDocumentsParams {})
                .await
                .map(|result| result.documents)
                .map_err(|error| crate::errors::describe(&error));
            self.agent_documents.set(Some(result));
        });
    }

    /// Does `action` to agent `id` (in `preset`); Customize and Open end
    /// on its document in the editor.
    pub fn agent_document_action(
        self,
        action: AgentDocumentAction,
        id: String,
        preset: Option<String>,
        path: Option<String>,
    ) {
        if action == AgentDocumentAction::Open {
            if let Some(path) = path {
                self.show_agent_document(&path);
            }
            return;
        }
        let Some(client) = self.client() else {
            return;
        };
        spawn_local(async move {
            let params = AgentDocumentParams {
                id: id.clone(),
                preset: preset.clone(),
            };
            let result = match action {
                AgentDocumentAction::Customize => client.customize_agent(params).await,
                AgentDocumentAction::Disable | AgentDocumentAction::Enable => {
                    client
                        .set_agent_enabled(AgentsSetEnabledParams {
                            id: id.clone(),
                            preset,
                            enabled: action == AgentDocumentAction::Enable,
                        })
                        .await
                }
                AgentDocumentAction::Reset => client.reset_agent(params).await,
                AgentDocumentAction::Replace => client.replace_agent(params).await,
                AgentDocumentAction::Open => return,
            };
            match result {
                Ok(result) => {
                    self.status.set(match action {
                        AgentDocumentAction::Customize => {
                            format!(
                                "{} inherits the official `{id}`: change the keys you want.",
                                result.path
                            )
                        }
                        AgentDocumentAction::Disable => format!("`{id}` is off."),
                        AgentDocumentAction::Enable => format!("`{id}` is on."),
                        AgentDocumentAction::Reset => format!("`{id}` is the official one again."),
                        _ => format!("{} holds a copy of the official `{id}`.", result.path),
                    });
                    // The configuration first, so the editor finds the
                    // document it opens.
                    if let Ok(config) = client.get_config().await {
                        self.config.set(Some(Ok(config)));
                    }
                    if matches!(
                        action,
                        AgentDocumentAction::Customize | AgentDocumentAction::Replace
                    ) {
                        // A new document is there to be edited.
                        self.agent_tab.set(AgentTab::File);
                        self.edit_config(&result.path);
                        self.sidebar_open.set(false);
                    }
                }
                Err(error) => self.status.set(crate::errors::describe(&error)),
            }
            self.poll_agent_documents();
        });
    }
}
