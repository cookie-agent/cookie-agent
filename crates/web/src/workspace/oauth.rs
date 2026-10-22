//! Signing in to MCP servers ([`crate::oauth`]): starting a sign-in from
//! the MCP sheet, finishing one with a callback (this tab's own, one
//! another tab handed over, or an address pasted), and cancelling one.

use cookie_agent_protocol::{McpAuthBeginParams, McpAuthCancelParams, McpAuthCompleteParams};
use leptos::{prelude::*, task::spawn_local};

use super::Workspace;
use crate::{
    browser,
    layout::{SettingsSection, View},
    oauth,
};

impl Workspace {
    /// Reads the sign-ins still waiting from the browser's storage.
    pub fn refresh_sign_ins(self) {
        self.sign_ins.set(oauth::waiting());
    }

    /// Starts signing in to `server`: the tab for the authorization page
    /// opens now, while the click counts, and is sent there once the daemon
    /// answers. A browser that blocks it gets this tab sent there instead.
    pub fn sign_in_mcp(self, server: String) {
        let Some(client) = self.client() else {
            return;
        };
        let Some(redirect_uri) = browser::page_href()
            .as_deref()
            .and_then(oauth::redirect_for_page)
        else {
            self.sign_in_outcome.set(Some(Err(
                "this page has no web address to come back to".into()
            )));
            return;
        };
        let tab = oauth::open_blank_tab();
        self.sign_in_outcome.set(None);
        spawn_local(async move {
            let began = client
                .begin_mcp_auth(McpAuthBeginParams {
                    server: server.clone(),
                    redirect_uri,
                })
                .await;
            match began {
                Ok(began) => {
                    let state = oauth::state_of(&began.authorization_url)
                        .unwrap_or_else(|| format!("server:{server}"));
                    oauth::record(&state, &server);
                    self.refresh_sign_ins();
                    self.status.set(format!(
                        "signing in to MCP server `{server}` in the browser"
                    ));
                    oauth::go_to(tab, &began.authorization_url);
                    self.poll_mcp();
                }
                Err(error) => {
                    if let Some(tab) = tab {
                        let _ = tab.close();
                    }
                    self.sign_in_outcome
                        .set(Some(Err(crate::errors::describe_full(&error))));
                }
            }
        });
    }

    /// Finishes the sign-in `state` to `server` with the address the
    /// browser landed on, once the page is connected; the MCP servers show
    /// how it ended.
    pub fn finish_sign_in(self, server: String, callback_url: String, state: Option<String>) {
        let Some(client) = self.client() else {
            self.sign_in_waiting
                .set_value(Some((server, callback_url, state)));
            return;
        };
        self.open_view(View::Settings, Some(SettingsSection::Mcp));
        self.sign_in_outcome.set(None);
        spawn_local(async move {
            let completed = client
                .complete_mcp_auth(McpAuthCompleteParams {
                    server: server.clone(),
                    callback_url,
                })
                .await;
            match completed {
                Ok(_) => {
                    if let Some(state) = &state {
                        oauth::forget(state);
                    }
                    self.sign_in_outcome.set(Some(Ok(server.clone())));
                    self.status
                        .set(format!("signed in to MCP server `{server}`"));
                }
                Err(error) => {
                    self.sign_in_outcome
                        .set(Some(Err(crate::errors::describe_full(&error))));
                    // A mistaken address leaves the daemon's flow waiting
                    // for the right one; anything else ended it.
                    let waiting = client.list_mcp_servers().await.is_ok_and(|listing| {
                        listing.servers.iter().any(|listed| {
                            listed.name == server
                                && cookie_agent_client_state::mcp::is_global(listed)
                                && listed.auth_in_progress == Some(true)
                        })
                    });
                    if !waiting && let Some(state) = &state {
                        oauth::forget(state);
                    }
                }
            }
            self.refresh_sign_ins();
            self.poll_mcp();
        });
    }

    /// The callback that arrived before the page had a connection.
    pub(super) fn finish_waiting_sign_in(self) {
        let mut waiting = None;
        self.sign_in_waiting
            .update_value(|held| waiting = held.take());
        if let Some((server, callback_url, state)) = waiting {
            self.finish_sign_in(server, callback_url, state);
        }
    }

    /// Cancels the sign-in `state` to `server`.
    pub fn cancel_sign_in(self, server: String, state: String) {
        oauth::forget(&state);
        self.refresh_sign_ins();
        self.sign_in_outcome.set(None);
        let Some(client) = self.client() else {
            return;
        };
        spawn_local(async move {
            let cancelled = client
                .cancel_mcp_auth(McpAuthCancelParams {
                    server: server.clone(),
                })
                .await;
            self.status.set(match cancelled {
                Ok(_) => format!("cancelled signing in to MCP server `{server}`"),
                Err(error) => format!(
                    "the sign-in to `{server}` had already ended: {}",
                    crate::errors::describe(&error)
                ),
            });
            self.poll_mcp();
        });
    }
}
