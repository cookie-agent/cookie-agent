//! `/share` (`docs/site/specs/conversation-shares.md` section 8): makes a
//! link that lets whoever opens it use the session as a guest, with the
//! defaults, and says its address.

use cookie_agent_client_state::shares::{ShareLifetime, link_address};
use cookie_agent_protocol::{SessionShareCreateParams, SessionShareResult};
use cookie_agent_protocol_transport::ClientError;

use super::{App, RpcUpdate};

impl App {
    /// Makes a link to the viewed session's root for the guest named
    /// `label`, or a guest with no name.
    pub(in crate::ui) fn run_share_command(&mut self, label: &str) {
        let Some(selected) = self.selected else {
            self.status = "open a session to share it".into();
            return;
        };
        let session_id = self.tree_root.unwrap_or(selected);
        let label = label.trim();
        let params = SessionShareCreateParams {
            session_id,
            label: (!label.is_empty()).then(|| label.to_owned()),
            can_send: true,
            expiry: ShareLifetime::default().expiry(jiff::Timestamp::now()),
        };
        let client = self.client.clone();
        let updates = self.rpc_updates_tx.clone();
        self.spawn_rpc(async move {
            let result = client.create_session_share(params).await;
            let _ = updates.send(RpcUpdate::SessionShared(Box::new(result)));
        });
    }

    /// Says the new link's address, or why there is none.
    pub(super) fn finish_session_share(&mut self, result: Result<SessionShareResult, ClientError>) {
        self.status = match result {
            Ok(result) => {
                let share = result.share;
                let address = match (&share.url, &self.page_url) {
                    (None, None) => {
                        format!("{} below the address of the daemon's web page", share.path)
                    }
                    (url, page) => link_address(
                        url.as_deref(),
                        &share.path,
                        page.as_deref().unwrap_or_default(),
                    ),
                };
                format!(
                    "link for {}: {address} (anyone with it uses this session as you do)",
                    share.label
                )
            }
            Err(error) => error.to_string(),
        };
    }
}
