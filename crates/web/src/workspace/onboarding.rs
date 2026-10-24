//! Onboarding (`docs/site/specs/default-models-and-onboarding.md` section
//! 9): the daemon's `onboarding.get` answer, the first-run sheet it opens
//! on a daemon that is not set up, and the calls the sheet makes. "Not now"
//! is this browser's choice; the daemon's answer stays what its
//! configuration says.

use cookie_agent_protocol::{
    OnboardingKitSource, OnboardingPreviewResult, OnboardingRunParams, OnboardingRunResult,
    OnboardingStatus,
};
use leptos::{prelude::*, task::spawn_local};

use super::Workspace;
use crate::browser;

/// Where this browser remembers "Not now": the page's origin is the
/// daemon's, so it is one choice per daemon.
pub const NOT_NOW_KEY: &str = "cookie.onboarding_not_now";

/// The first-run sheet's state that outlives a render.
#[derive(Clone, Copy)]
pub struct Onboarding {
    /// `onboarding.get`'s answer; `None` before it or from a daemon
    /// without onboarding.
    pub status: RwSignal<Option<OnboardingStatus>>,
    pub open: RwSignal<bool>,
    /// `onboarding.run` is in flight.
    pub running: RwSignal<bool>,
    /// What the last run did, for the summary that ends the sheet.
    pub result: RwSignal<Option<OnboardingRunResult>>,
    /// Why the last run was refused.
    pub error: RwSignal<Option<String>>,
}

impl Default for Onboarding {
    fn default() -> Self {
        Self {
            status: RwSignal::new(None),
            open: RwSignal::new(false),
            running: RwSignal::new(false),
            result: RwSignal::new(None),
            error: RwSignal::new(None),
        }
    }
}

/// Whether the sheet opens by itself for `status` on connecting: the daemon
/// is not set up and this browser did not say "Not now".
#[must_use]
pub fn opens_on_connect(status: &OnboardingStatus, not_now: bool) -> bool {
    !status.ready && !not_now
}

/// Whether an `onboarding.changed` closes an open sheet: the daemon is set
/// up now and this page is not showing its own run's summary.
#[must_use]
pub fn closes_on_change(status: &OnboardingStatus, running: bool, summary: bool) -> bool {
    status.ready && !running && !summary
}

impl Workspace {
    /// Reads `onboarding.get` on connecting and opens the sheet when the
    /// daemon is not set up, unless this browser said "Not now". A daemon
    /// without onboarding answers with an error, which leaves it closed.
    pub(super) fn refresh_onboarding(self, on_connect: bool) {
        let Some(client) = self.client() else {
            return;
        };
        let read = self.begin_read(|reads| &mut reads.onboarding);
        spawn_local(async move {
            let answer = client.get_onboarding().await;
            // An `onboarding.changed` since the read was asked for is newer
            // than its answer: on connecting, the sheet opens by that.
            let answer = if self.read_is_current(|reads| &mut reads.onboarding, read) {
                answer
            } else if let Some(status) = self.onboarding.status.get_untracked() {
                Ok(status)
            } else {
                return;
            };
            match answer {
                Ok(status) => {
                    let opens = on_connect
                        && opens_on_connect(&status, browser::load(NOT_NOW_KEY).is_some());
                    self.onboarding.status.set(Some(status));
                    if opens {
                        self.onboarding.result.set(None);
                        self.onboarding.open.set(true);
                    }
                }
                Err(_) => self.onboarding.status.set(None),
            }
        });
    }

    /// `onboarding.changed`: the new answer. Finishing in another tab closes
    /// the sheet here.
    pub(super) fn apply_onboarding_change(self, status: OnboardingStatus) {
        self.supersede_reads(|reads| &mut reads.onboarding);
        let closes = closes_on_change(
            &status,
            self.onboarding.running.get_untracked(),
            self.onboarding.result.with_untracked(Option::is_some),
        );
        self.onboarding.status.set(Some(status));
        if closes {
            self.onboarding.open.set(false);
        }
    }

    /// Opens the sheet: Settings' "Set up Cookie".
    pub fn open_onboarding(self) {
        self.onboarding.result.set(None);
        self.onboarding.error.set(None);
        self.onboarding.open.set(true);
        self.refresh_onboarding(false);
    }

    /// "Not now": closes the sheet and keeps it closed on later visits.
    pub fn dismiss_onboarding(self) {
        browser::store(NOT_NOW_KEY, "1");
        self.onboarding.open.set(false);
    }

    /// Closes the sheet, as its summary's Done does.
    pub fn close_onboarding(self) {
        self.onboarding.open.set(false);
        self.onboarding.result.set(None);
    }

    /// `onboarding.preview` of `source`.
    pub async fn preview_kit(
        self,
        source: Option<OnboardingKitSource>,
    ) -> Result<OnboardingPreviewResult, String> {
        let client = self.client().ok_or("not connected to the daemon")?;
        client
            .preview_onboarding(source)
            .await
            .map_err(|error| crate::errors::describe(&error))
    }

    /// `onboarding.run`; the summary on success, the reason in the sheet
    /// otherwise.
    pub fn run_onboarding(self, params: OnboardingRunParams) {
        let Some(client) = self.client() else {
            return;
        };
        self.onboarding.running.set(true);
        self.onboarding.error.set(None);
        spawn_local(async move {
            match client.run_onboarding(params).await {
                Ok(result) => {
                    self.onboarding.status.set(Some(result.status.clone()));
                    self.onboarding.result.set(Some(result));
                    browser::forget(NOT_NOW_KEY);
                }
                Err(error) => self
                    .onboarding
                    .error
                    .set(Some(crate::errors::describe(&error))),
            }
            self.onboarding.running.set(false);
        });
    }

    /// "Start your first session": a new-session draft, on `primary` when
    /// the daemon has it.
    pub fn start_first_session(self) {
        self.close_onboarding();
        self.start_new_session();
        if let Some(primary) = untrack(|| self.selectable_agents())
            .into_iter()
            .find(|agent| agent.id.as_str() == "primary")
        {
            self.pick_agent(&primary);
        }
    }
}

#[cfg(test)]
mod tests {
    use cookie_agent_protocol::{OnboardingRequirement, OnboardingStatus};

    use super::{closes_on_change, opens_on_connect};

    fn status(ready: bool) -> OnboardingStatus {
        OnboardingStatus {
            ready,
            missing: if ready {
                Vec::new()
            } else {
                vec![OnboardingRequirement::Provider]
            },
            providers_connected: Vec::new(),
            default_model: None,
            small_model: None,
        }
    }

    #[test]
    fn the_sheet_opens_on_a_daemon_not_set_up_unless_not_now() {
        assert!(opens_on_connect(&status(false), false));
        assert!(!opens_on_connect(&status(false), true));
        assert!(!opens_on_connect(&status(true), false));
    }

    #[test]
    fn finishing_elsewhere_closes_the_sheet_but_not_this_page_s_summary() {
        assert!(closes_on_change(&status(true), false, false));
        assert!(!closes_on_change(&status(true), true, false));
        assert!(!closes_on_change(&status(true), false, true));
        assert!(!closes_on_change(&status(false), false, false));
    }
}
