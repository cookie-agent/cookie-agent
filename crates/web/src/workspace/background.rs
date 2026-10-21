//! Background calls: moving a running call to the background
//! (`run.tool_background`), the bar while the session has some, and the
//! `/background` sheet that stops one. The list comes from
//! `session.background.list` after subscribing and `background.changed`.

use cookie_agent_protocol::{BackgroundCall, RunToolBackgroundParams, ToolCallId};
use leptos::{prelude::*, task::spawn_local};

use super::{Panel, Workspace};

impl Workspace {
    /// The viewed session's background calls.
    pub fn background_calls(self) -> Vec<BackgroundCall> {
        let Some(session_id) = self.selected.get() else {
            return Vec::new();
        };
        self.with_store(|store| {
            store
                .sessions
                .get(&session_id)
                .map(|state| state.background_calls.clone())
                .unwrap_or_default()
        })
    }

    /// Opens the background calls sheet.
    pub(super) fn open_background(self) {
        // Empty, it says no call runs in the background.
        self.panel.set(Some(Panel::Background));
    }

    /// Asks the daemon to move the running call `call_id` to the background.
    pub fn request_background(self, call_id: ToolCallId) {
        let (Some(client), Some(run_id)) = (self.client(), self.active_run()) else {
            self.status.set("the call is no longer running".into());
            return;
        };
        self.status.set("moving the call to the background…".into());
        spawn_local(async move {
            let status = match client
                .background_tool(RunToolBackgroundParams { run_id, call_id })
                .await
            {
                Ok(result) if result.accepted => "the call moves to the background once it can",
                Ok(_) => "this call cannot move to the background",
                Err(error) => {
                    self.status.set(crate::errors::describe(&error));
                    return;
                }
            };
            self.status.set(status.into());
        });
    }

    /// Stops the background call `job` as the user; it still reports to the
    /// model.
    pub fn stop_background(self, job: String) {
        let (Some(client), Some(session_id)) = (self.client(), self.selected.get_untracked())
        else {
            return;
        };
        self.status.set(format!("stopping {job}…"));
        spawn_local(async move {
            match client.stop_background(session_id, job.clone()).await {
                Ok(_) => self.status.set(format!("stopped {job}")),
                Err(error) => self.status.set(crate::errors::describe(&error)),
            }
        });
    }
}
