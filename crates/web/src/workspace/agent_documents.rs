//! The Agents view's panel (`docs/site/specs/acp-onboarding.md` section 7):
//! `agents.documents`, and the changes the panel makes, each followed by a
//! fresh read of the documents and the configuration.

use cookie_agent_client_state::agent_documents::AgentDocumentAction;
use cookie_agent_protocol::{AgentDocumentParams, AgentsDocumentsParams, AgentsSetEnabledParams};
use leptos::{prelude::*, task::spawn_local};

use super::Workspace;

impl Workspace {
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
                self.edit_config(&path);
                self.sidebar_open.set(false);
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
