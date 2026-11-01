//! Pinned sessions and the session list's filter
//! (`docs/site/specs/session-pins-and-filters.md`): the daemon keeps the
//! pins and announces each change, which the list shows; the filter's date
//! is the list's window, read again from the daemon when it changes, and
//! its agents and machines narrow what the list holds.

use cookie_agent_client_state::session_filter::ActiveWithin;
use cookie_agent_protocol::{SessionId, SessionMeta};
use leptos::{prelude::*, task::spawn_local};

use super::Workspace;

impl Workspace {
    /// Pins or unpins `sessions`; the status line says how many changed.
    pub fn set_pinned(self, sessions: Vec<SessionId>, pinned: bool) {
        let Some(client) = self.client() else {
            return;
        };
        spawn_local(async move {
            match client.set_session_pinned(sessions, pinned).await {
                Ok(result) => self
                    .status
                    .set(cookie_agent_client_state::pins::pinned_message(
                        result.changed.len(),
                        pinned,
                    )),
                Err(error) => self.status.set(crate::errors::describe(&error)),
            }
        });
    }

    /// Lists the sessions active `within`, with every pinned, unread, and
    /// busy one: the window starts over there, and the list is read again.
    /// A range's days are the browser's.
    pub fn set_active_within(self, within: ActiveWithin) {
        if self.session_filter.with_untracked(|filter| filter.within) == within {
            return;
        }
        self.session_rollback.set(None);
        self.session_filter.update(|filter| filter.within = within);
        self.session_window
            .set_value(within.window(&crate::cron::zone_at(jiff::Timestamp::now())));
        let Some(client) = self.client() else {
            return;
        };
        spawn_local(async move { self.read_session_list(&client).await });
    }

    /// The filter back to its default, reading the list again when its
    /// date changes; the workspace filter goes with it.
    pub fn reset_session_filter(self) {
        self.directory_filter.set(None);
        let within = ActiveWithin::default();
        self.session_filter.update(|filter| {
            filter.agents.clear();
            filter.machines.clear();
        });
        self.set_active_within(within);
    }

    /// How many of the filter's parts narrow the list beyond the default:
    /// a date other than the default's, agents, machines, a workspace.
    pub fn filter_changes(self) -> usize {
        self.session_filter.with(|filter| filter.changed())
            + usize::from(self.directory_filter.with(Option::is_some))
    }

    /// Whether the filter leaves `session` in the list: a range's days (the
    /// daemon's answer holds only those, but a session announced since may
    /// be newer), and its agents and machines; tracks the filter, and the
    /// runtimes once a machine is chosen.
    pub fn filter_passes(self, session: &SessionMeta) -> bool {
        self.session_filter.with(|filter| {
            if let ActiveWithin::Between(range) = filter.within
                && let Some(bounds) = range.bounds(&crate::cron::zone_at(jiff::Timestamp::now()))
                && !cookie_agent_client_state::session_filter::in_range(session, bounds)
            {
                return false;
            }
            if filter.by_date_only() {
                return true;
            }
            let machines = if filter.machines.is_empty() {
                Vec::new()
            } else {
                self.runtimes.with(|listing| {
                    cookie_agent_client_state::badges::session_machines(session, listing.as_ref())
                })
            };
            filter.matches(session, &machines)
        })
    }
}

/// The browser's date today.
pub(crate) fn local_today() -> jiff::civil::Date {
    let now = jiff::Timestamp::now();
    now.to_zoned(crate::cron::zone_at(now)).date()
}

/// The browser's date at `at`.
pub(crate) fn local_date(at: jiff::Timestamp) -> jiff::civil::Date {
    at.to_zoned(crate::cron::zone_at(at)).date()
}

/// `date` as the list names a day: "Oct 3", with the year when it is not
/// `today`'s.
pub(crate) fn date_label(date: jiff::civil::Date, today: jiff::civil::Date) -> String {
    cookie_agent_client_state::session_filter::DateRange::new(date, date).label(today)
}

impl Workspace {
    /// The first day a rolled-back list shows: the filter's days before the
    /// newest session's last activity; `None` while it is not rolled back.
    pub fn rollback_start(self) -> Option<jiff::civil::Date> {
        let newest = self.session_rollback.get()?;
        let days = self
            .session_filter
            .with(|filter| filter.within.days())
            .unwrap_or_default();
        let start = newest
            .checked_sub(jiff::SignedDuration::from_hours(24 * days))
            .ok()?;
        Some(local_date(start))
    }
}
