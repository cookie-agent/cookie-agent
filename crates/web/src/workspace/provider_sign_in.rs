//! A provider's browser sign-in from the connect sheet. The daemon always
//! listens for the redirect itself, so a browser on the daemon's machine
//! finishes on its own. A page served from loopback is on that machine; any
//! other page also offers to paste the address the sign-in tab could not
//! load, for a browser elsewhere.

use cookie_agent_protocol::{
    ProviderAuthBeginParams, ProviderAuthCancelParams, ProviderAuthCompleteParams,
    ProviderDescriptor, ProviderId,
};
use leptos::{prelude::*, task::spawn_local};

use super::Workspace;
use crate::{browser, oauth};

/// Where one provider's sign-in stands in the connect sheet.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub enum ProviderSignIn {
    #[default]
    Idle,
    /// Waiting for the user to finish in the sign-in tab.
    Waiting {
        /// The address the sign-in tab shows, to open it again.
        authorization_url: String,
        /// The browser shares the daemon's machine, so the daemon surely
        /// catches the redirect; nothing to paste.
        caught_by_daemon: bool,
    },
    Failed(String),
}

/// Whether the page is served from this machine's loopback, the same
/// machine as a local daemon.
fn page_on_loopback() -> bool {
    browser::page_href()
        .and_then(|href| url::Url::parse(&href).ok())
        .and_then(|url| url.host_str().map(str::to_owned))
        .is_some_and(|host| matches!(host.as_str(), "127.0.0.1" | "localhost" | "[::1]"))
}

impl Workspace {
    /// Starts signing in to `provider`. The sign-in tab opens now, while the
    /// click counts, and is sent to the authorization page once the daemon
    /// answers.
    pub fn sign_in_provider(self, provider: ProviderDescriptor, state: RwSignal<ProviderSignIn>) {
        let Some(client) = self.client() else {
            return;
        };
        let Some(method) = cookie_agent_client_state::providers::sign_in_method(&provider) else {
            return;
        };
        let caught_by_daemon = page_on_loopback();
        let params = ProviderAuthBeginParams {
            provider_id: provider.id.clone(),
            method: method.id.clone(),
            redirect_uri: None,
        };
        let tab = oauth::open_blank_tab();
        state.set(ProviderSignIn::Idle);
        spawn_local(async move {
            match client.begin_provider_auth(params).await {
                Ok(began) => {
                    self.status.set(format!(
                        "signing in to {} in the browser",
                        began.provider_id
                    ));
                    oauth::go_to(tab, &began.authorization_url);
                    state.set(ProviderSignIn::Waiting {
                        authorization_url: began.authorization_url,
                        caught_by_daemon,
                    });
                }
                Err(error) => {
                    if let Some(tab) = tab {
                        let _ = tab.close();
                    }
                    state.set(ProviderSignIn::Failed(crate::errors::describe_full(&error)));
                }
            }
        });
    }

    /// Finishes the sign-in with the address the sign-in tab could not load,
    /// then calls `finished`.
    pub fn finish_provider_sign_in(
        self,
        provider_id: ProviderId,
        callback_url: String,
        state: RwSignal<ProviderSignIn>,
        finished: impl FnOnce() + 'static,
    ) {
        let Some(client) = self.client() else {
            return;
        };
        spawn_local(async move {
            match client
                .complete_provider_auth(ProviderAuthCompleteParams {
                    provider_id: provider_id.clone(),
                    callback_url,
                })
                .await
            {
                Ok(completed) => {
                    state.set(ProviderSignIn::Idle);
                    finished();
                    self.status
                        .set(completed.account.email.as_ref().map_or_else(
                            || format!("signed in to {provider_id}"),
                            |email| format!("signed in to {provider_id} as {email}"),
                        ));
                }
                Err(error) => {
                    state.set(ProviderSignIn::Failed(crate::errors::describe_full(&error)));
                }
            }
        });
    }

    /// Ends a sign-in in progress.
    pub fn cancel_provider_sign_in(self, provider_id: ProviderId, state: RwSignal<ProviderSignIn>) {
        state.set(ProviderSignIn::Idle);
        let Some(client) = self.client() else {
            return;
        };
        spawn_local(async move {
            let _ = client
                .cancel_provider_auth(ProviderAuthCancelParams { provider_id })
                .await;
        });
    }
}
