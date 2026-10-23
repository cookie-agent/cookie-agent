//! Restarting the daemon (`daemon.restart`, client protocol 1.19): the
//! banner while its configuration waits for a restart, the confirmation,
//! and the wait until the connection closes. The views are
//! `crate::views::restart`.

use cookie_agent_client_state::restart::{RestartOutcome, restart_banner, restarting_text};
use cookie_agent_protocol::{
    ConfigStatus, DaemonRestartingNotification, RestartWhen, RuntimeChangeReason,
};
use leptos::{prelude::*, task::spawn_local};

use super::Workspace;

impl Workspace {
    /// Reads the configuration status again, for the banner.
    pub(super) fn refresh_config_status(self) {
        if !self.can_restart.get_untracked() {
            return;
        }
        let Some(client) = self.client() else {
            return;
        };
        let connection = self.connection.get_value();
        spawn_local(async move {
            if let Ok(config) = client.get_config().await
                && self.is_current(connection)
            {
                self.config_status.set(Some(config.status));
            }
        });
    }

    /// Notes a status `config.get`, `config.apply`, or `config.reload`
    /// answered.
    pub(super) fn note_config_status(self, status: &ConfigStatus) {
        self.config_status.set(Some(status.clone()));
    }

    /// A runtime change from a configuration change: the status may have
    /// changed with it.
    pub(super) fn config_may_have_changed(self, reasons: &[RuntimeChangeReason]) {
        if reasons.iter().any(|reason| {
            matches!(
                reason,
                RuntimeChangeReason::ConfigReloaded
                    | RuntimeChangeReason::AgentReloaded
                    | RuntimeChangeReason::Resync
            )
        }) {
            self.refresh_config_status();
        }
    }

    /// The banner's text while the configuration waits for a restart.
    pub fn restart_banner(self) -> Option<String> {
        if !self.can_restart.get() {
            return None;
        }
        self.config_status
            .with(|status| status.as_ref().and_then(restart_banner))
    }

    /// Opens the restart confirmation.
    pub fn ask_restart(self) {
        self.restart_confirm.set(true);
    }

    /// Asks the daemon to restart `when` chosen.
    pub fn restart(self, when: RestartWhen) {
        self.restart_confirm.set(false);
        let Some(client) = self.client() else {
            return;
        };
        spawn_local(async move {
            let outcome = RestartOutcome::from_result(when, client.restart_daemon(when).await);
            let status = outcome.status();
            if matches!(outcome, RestartOutcome::Scheduled { .. }) {
                // Until a notification says more, what the answer said.
                if self.restarting.get_untracked().is_none() {
                    self.restarting.set(Some(status.clone()));
                }
            }
            self.status.set(status);
        });
    }

    /// `daemon.restarting`: the wait goes on until the connection closes.
    pub(super) fn note_restarting(self, restarting: &DaemonRestartingNotification) {
        self.restarting.set(Some(restarting_text(restarting)));
    }
}
