//! The Forwards view's calls (`forward*`, client protocol 1.109.0,
//! `docs/site/specs/forwards.md` section 8.1): the listing, kept fresh by
//! `forwards.changed`, the New forward form, a forward's expiry, share
//! links, and Delete. The view is `crate::views::forwards`.

use cookie_agent_client_state::{forward::ForwardLifetime, shares::ShareLifetime};
use cookie_agent_protocol::{
    Forward, ForwardCreateParams, ForwardShare, ForwardShareCreateParams, ForwardUpdateParams,
};
use leptos::{prelude::*, task::spawn_local};

use super::Workspace;
use crate::layout::View;

/// An open forward's tabs.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ForwardTab {
    /// The route, the address, the expiry, and the share links.
    Overview,
    /// The recent connections.
    Connections,
}

impl Workspace {
    /// Reads `forwards.list`, when the daemon keeps forwards: every one for
    /// the user, the shared session's for a guest.
    pub fn poll_forwards(self) {
        if !self.has_forwards.get_untracked() {
            return;
        }
        let Some(client) = self.client() else {
            return;
        };
        let session = self
            .guest
            .with_untracked(|guest| guest.as_ref().map(|guest| guest.session_id));
        let read = self.begin_read(|reads| &mut reads.forwards);
        spawn_local(async move {
            let result = client
                .list_forwards(session)
                .await
                .map_err(|error| crate::errors::describe_full(&error));
            if self.read_is_current(|reads| &mut reads.forwards, read) {
                self.forwards.set(Some(result));
            }
        });
    }

    /// Opening the view: the list, with no forward or form open.
    pub(super) fn open_forwards(self) {
        self.forward_form.set(false);
        self.forward_selected.set(None);
        self.forward_confirm.set(None);
        self.forward_error.set(None);
        self.poll_forwards();
        self.refresh_shares();
    }

    /// Opens the view at forward `handle`'s page.
    pub fn open_forward(self, handle: String) {
        if self.view.get_untracked() != View::Forwards {
            self.open_view(View::Forwards, None);
        }
        self.show_forward(handle);
    }

    /// Shows forward `handle`'s page, on its first tab.
    pub fn show_forward(self, handle: String) {
        self.forward_form.set(false);
        self.forward_error.set(None);
        self.forward_tab.set(ForwardTab::Overview);
        self.forward_selected.set(Some(handle));
    }

    /// The forwards listed, if they are.
    pub fn forward_list(self) -> Vec<Forward> {
        self.forwards.with(|forwards| {
            forwards
                .as_ref()
                .and_then(|forwards| forwards.as_ref().ok())
                .map(|forwards| forwards.forwards.clone())
                .unwrap_or_default()
        })
    }

    /// Where a web forward's app is from this page: below the daemon's
    /// page for the user, below the share for a guest.
    pub fn forward_url(self, forward: &Forward) -> Option<String> {
        let address = forward.address.as_deref()?;
        Some(format!("{}{address}", self.share_page_base.get_untracked()))
    }

    /// The share links of forward `handle`, newest first, as the Sharing
    /// view last read them.
    pub fn forward_links(self, handle: &str) -> Vec<ForwardShare> {
        self.share_list.with(|list| {
            let mut links: Vec<ForwardShare> = list
                .as_ref()
                .and_then(|list| list.as_ref().ok())
                .map(|list| {
                    list.forwards
                        .iter()
                        .filter(|link| link.forward == handle)
                        .cloned()
                        .collect()
                })
                .unwrap_or_default();
            links.sort_by_key(|link| std::cmp::Reverse(link.created.at));
            links
        })
    }

    /// The address a forward's share link is copied as.
    pub fn forward_share_address(self, share: &ForwardShare) -> String {
        cookie_agent_client_state::shares::link_address(
            share.url.as_deref(),
            &share.path,
            &self.share_page_base.get_untracked(),
        )
    }

    /// Opens the New forward form.
    pub fn new_forward(self) {
        if self.view.get_untracked() != View::Forwards {
            self.open_view(View::Forwards, None);
        }
        self.forward_selected.set(None);
        self.forward_error.set(None);
        self.forward_form.set(true);
    }

    /// Sends the form: the new forward's page opens.
    pub fn create_forward(self, params: ForwardCreateParams) {
        let Some(client) = self.client() else {
            return;
        };
        self.forward_error.set(None);
        spawn_local(async move {
            match client.create_forward(params).await {
                Ok(made) => {
                    self.poll_forwards();
                    self.show_forward(made.forward.handle);
                }
                Err(error) => self
                    .forward_error
                    .set(Some(crate::errors::describe(&error))),
            }
        });
    }

    /// Gives forward `handle` a new expiry, counted from now.
    pub fn set_forward_lifetime(self, handle: String, lifetime: ForwardLifetime) {
        let Some(client) = self.client() else {
            return;
        };
        spawn_local(async move {
            match client
                .update_forward(ForwardUpdateParams {
                    forward: handle,
                    title: None,
                    expires_in_hours: Some(lifetime.hours()),
                })
                .await
            {
                Ok(_) => {
                    self.status.set(format!(
                        "the forward now {}",
                        match lifetime {
                            ForwardLifetime::Never => "never ends".to_owned(),
                            other => format!("ends in {}", other.label()),
                        }
                    ));
                    self.poll_forwards();
                }
                Err(error) => self
                    .forward_error
                    .set(Some(crate::errors::describe(&error))),
            }
        });
    }

    /// Stops and deletes forward `handle`; its share links end.
    pub fn delete_forward(self, handle: String) {
        let Some(client) = self.client() else {
            return;
        };
        spawn_local(async move {
            match client.delete_forward(handle.clone()).await {
                Ok(_) => {
                    if self.forward_selected.get_untracked().as_deref() == Some(&handle) {
                        self.forward_selected.set(None);
                    }
                    self.status.set(format!("deleted {handle}"));
                    self.poll_forwards();
                    self.refresh_shares();
                }
                Err(error) => self
                    .forward_error
                    .set(Some(crate::errors::describe(&error))),
            }
        });
    }

    /// Makes a share link to web forward `handle` and copies it.
    pub fn share_forward(self, handle: String, lifetime: ShareLifetime) {
        let Some(client) = self.client() else {
            return;
        };
        spawn_local(async move {
            match client
                .create_forward_share(ForwardShareCreateParams {
                    forward: handle,
                    expiry: lifetime.expiry(jiff::Timestamp::now()),
                    reuse: false,
                })
                .await
            {
                Ok(made) => {
                    let address = self.forward_share_address(&made.share);
                    let copied = crate::browser::copy_text(&address).await;
                    self.status.set(if copied {
                        "link made and copied".to_owned()
                    } else {
                        format!("link made: {address}")
                    });
                    self.refresh_shares();
                }
                Err(error) => self
                    .forward_error
                    .set(Some(crate::errors::describe(&error))),
            }
        });
    }
}
