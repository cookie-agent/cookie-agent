//! The session list's filter (`docs/site/specs/session-pins-and-filters.md`):
//! how far back the list reaches, and which agents and machines its
//! sessions run on. The date is the list's window
//! ([`crate::session_window`]), which the daemon answers with every pinned,
//! unread, and busy session too; the agents and the machines narrow what
//! the list holds, those sessions included.

use std::collections::BTreeSet;

use cookie_agent_protocol::{SessionMeta, SessionOrigin};
use jiff::{Timestamp, civil::Date, tz::TimeZone};

use crate::session_window::SessionWindow;

/// How far back the list reaches: a number of days back from now, every
/// session, or the days between two dates.
#[derive(Clone, Copy, Debug, Default, Eq, Hash, PartialEq)]
pub enum ActiveWithin {
    Day,
    #[default]
    Week,
    Month,
    Quarter,
    /// Every session.
    Ever,
    /// The sessions last active on the days of a range.
    Between(DateRange),
}

impl ActiveWithin {
    /// The choices that reach back from now, in the order the filter
    /// offers them; a range comes after them.
    pub const ALL: [Self; 5] = [
        Self::Day,
        Self::Week,
        Self::Month,
        Self::Quarter,
        Self::Ever,
    ];

    /// The days it reaches back from now; `None`: every session, or a
    /// range.
    #[must_use]
    pub const fn days(self) -> Option<i64> {
        match self {
            Self::Day => Some(1),
            Self::Week => Some(7),
            Self::Month => Some(30),
            Self::Quarter => Some(90),
            Self::Ever | Self::Between(_) => None,
        }
    }

    /// Its name in the filter.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Day => "24 hours",
            Self::Week => "7 days",
            Self::Month => "30 days",
            Self::Quarter => "90 days",
            Self::Ever => "All time",
            Self::Between(_) => "Dates…",
        }
    }

    /// What the list's summary says of it, `today` being the browser's
    /// date: "Last 7 days", or a range's dates.
    #[must_use]
    pub fn summary(self, today: Date) -> String {
        match self {
            Self::Day => "Last 24 hours".to_owned(),
            Self::Week => "Last 7 days".to_owned(),
            Self::Month => "Last 30 days".to_owned(),
            Self::Quarter => "Last 90 days".to_owned(),
            Self::Ever => "All time".to_owned(),
            Self::Between(range) => range.label(today),
        }
    }

    /// The summary short enough to share a line with more: "7 days", or a
    /// range's dates.
    #[must_use]
    pub fn short(self, today: Date) -> String {
        match self {
            Self::Between(range) => range.label(today),
            preset => preset.label().to_owned(),
        }
    }

    /// A stable name for it, as a page's `data-` attribute says it.
    #[must_use]
    pub const fn id(self) -> &'static str {
        match self {
            Self::Day => "day",
            Self::Week => "week",
            Self::Month => "month",
            Self::Quarter => "quarter",
            Self::Ever => "ever",
            Self::Between(_) => "between",
        }
    }

    /// The list's window for it, a range's days counted in `zone`.
    #[must_use]
    pub fn window(self, zone: &TimeZone) -> SessionWindow {
        match self {
            Self::Between(range) => match range.bounds(zone) {
                Some((low, high)) => SessionWindow::between(low, high),
                None => SessionWindow::recent(None),
            },
            preset => SessionWindow::recent(preset.days()),
        }
    }
}

/// The days from one date to another, both included.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct DateRange {
    pub from: Date,
    pub to: Date,
}

impl DateRange {
    /// The days between `one` and `other`, whichever comes first.
    #[must_use]
    pub fn new(one: Date, other: Date) -> Self {
        Self {
            from: one.min(other),
            to: one.max(other),
        }
    }

    /// The last `days` days up to `today`, both included.
    #[must_use]
    pub fn ending(today: Date, days: i64) -> Self {
        let from = today
            .checked_sub(jiff::Span::new().days(days.max(1) - 1))
            .unwrap_or(today);
        Self::new(from, today)
    }

    /// From the start of the first day to the start of the day after the
    /// last, in `zone`; `None` past the ends of time.
    #[must_use]
    pub fn bounds(self, zone: &TimeZone) -> Option<(Timestamp, Timestamp)> {
        let low = self.from.to_zoned(zone.clone()).ok()?.timestamp();
        let high = self
            .to
            .tomorrow()
            .ok()?
            .to_zoned(zone.clone())
            .ok()?
            .timestamp();
        Some((low, high))
    }

    /// "Oct 3 – Oct 10", "Oct 3" for one day, with the year when either
    /// date is not in `today`'s.
    #[must_use]
    pub fn label(self, today: Date) -> String {
        let format = if self.from.year() == today.year() && self.to.year() == today.year() {
            "%b %-d"
        } else {
            "%b %-d, %Y"
        };
        let from = self.from.strftime(format).to_string();
        if self.from == self.to {
            return from;
        }
        format!("{from} – {}", self.to.strftime(format))
    }
}

/// Whether the list keeps `session` for a range ending at `high` and
/// starting at `low`: pinned, unread, and busy sessions whatever their age,
/// as the daemon lists them, and the rest only when last active in it, so
/// that a session announced since (one just created, say) stays out of a
/// range in the past.
#[must_use]
pub fn in_range(session: &SessionMeta, (low, high): (Timestamp, Timestamp)) -> bool {
    crate::rows::listed_first(session)
        || session.unread
        || (low <= session.last_activity && session.last_activity < high)
}

/// What the list shows: sessions active within [`Self::within`], run by
/// one of [`Self::agents`] (any while empty), on one of
/// [`Self::machines`] (any while empty).
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct SessionFilter {
    pub within: ActiveWithin,
    /// Agent IDs, as `creation_selection.agent` names them.
    pub agents: BTreeSet<String>,
    /// Machine names, as [`crate::badges::session_machines`] gives them.
    pub machines: BTreeSet<String>,
}

impl SessionFilter {
    /// Whether `session`, working on `machines`, passes the agents and the
    /// machines. The date is the daemon's to apply.
    #[must_use]
    pub fn matches(&self, session: &SessionMeta, machines: &[String]) -> bool {
        (self.agents.is_empty()
            || self
                .agents
                .contains(session.creation_selection.agent.as_str()))
            && (self.machines.is_empty()
                || machines
                    .iter()
                    .any(|machine| self.machines.contains(machine)))
    }

    /// Whether only the date narrows the list: no agent or machine is
    /// chosen.
    #[must_use]
    pub fn by_date_only(&self) -> bool {
        self.agents.is_empty() && self.machines.is_empty()
    }

    /// How many of its parts differ from the default filter's.
    #[must_use]
    pub fn changed(&self) -> usize {
        usize::from(self.within != ActiveWithin::default())
            + usize::from(!self.agents.is_empty())
            + usize::from(!self.machines.is_empty())
    }

    /// Chooses `agent`, or lets it go when chosen.
    pub fn toggle_agent(&mut self, agent: &str) {
        if !self.agents.remove(agent) {
            self.agents.insert(agent.to_owned());
        }
    }

    /// Chooses `machine`, or lets it go when chosen.
    pub fn toggle_machine(&mut self, machine: &str) {
        if !self.machines.remove(machine) {
            self.machines.insert(machine.to_owned());
        }
    }
}

/// The agents a filter offers: those that run a listed root session, and
/// those `chosen` already, so each can be let go; sorted, each once.
#[must_use]
pub fn agent_options<'a>(
    sessions: impl IntoIterator<Item = &'a SessionMeta>,
    chosen: &BTreeSet<String>,
) -> Vec<String> {
    let mut options = chosen.clone();
    options.extend(
        sessions
            .into_iter()
            .filter(|session| matches!(session.origin, SessionOrigin::Root))
            .map(|session| session.creation_selection.agent.to_string()),
    );
    options.into_iter().collect()
}

/// The machines a filter offers: those `machines` names for the listed
/// sessions, and those `chosen` already; sorted, each once.
#[must_use]
pub fn machine_options<I, M>(machines: I, chosen: &BTreeSet<String>) -> Vec<String>
where
    I: IntoIterator<Item = M>,
    M: IntoIterator<Item = String>,
{
    let mut options = chosen.clone();
    options.extend(machines.into_iter().flatten());
    options.into_iter().collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn session(agent: &str, delegated: bool) -> SessionMeta {
        let origin = if delegated {
            serde_json::json!({
                "type": "delegated",
                "root_session_id": "0190a5b2-0000-7000-8000-000000000001",
                "parent_session_id": "0190a5b2-0000-7000-8000-000000000001",
                "parent_run_id": "0190a5b2-0000-7000-8000-0000000000a1",
                "parent_tool_call_id": "0190a5b2-0000-7000-8000-0000000000a2",
                "invocation_id": "0190a5b2-0000-7000-8000-0000000000b1",
                "depth": 1
            })
        } else {
            serde_json::json!({"type": "root"})
        };
        serde_json::from_value(serde_json::json!({
            "session_id": cookie_agent_protocol::SessionId::new_v7(),
            "origin": origin,
            "short_id": "test",
            "cwd_identity": "/w",
            "title": null,
            "creation_selection": {
                "agent": agent,
                "model": {"model": "custom.local/test", "variant": null},
            },
            "runtime_revision": format!("sha256:{}", "1".repeat(64)),
            "catalog_revision": format!("sha256:{}", "2".repeat(64)),
            "provider_state_revision": format!("sha256:{}", "3".repeat(64)),
            "model_revision": format!("sha256:{}", "4".repeat(64)),
            "agent_revision": format!("sha256:{}", "5".repeat(64)),
            "recipe_registry_revision": format!("sha256:{}", "6".repeat(64)),
            "manifest_revision": format!("sha256:{}", "7".repeat(64)),
            "title_updated_seq": 0,
            "last_event_seq": 1,
            "last_activity": "2026-10-01T00:00:00Z",
            "status": "idle",
            "skipped_events": [],
        }))
        .expect("session meta")
    }

    fn names(names: &[&str]) -> Vec<String> {
        names.iter().map(|name| (*name).to_owned()).collect()
    }

    fn date(text: &str) -> Date {
        text.parse().expect("test date")
    }

    fn at(text: &str) -> Timestamp {
        text.parse().expect("test time")
    }

    #[test]
    fn a_range_runs_from_its_first_days_start_to_its_last_days_end() {
        let range = DateRange::new(date("2026-09-10"), date("2026-09-03"));
        assert_eq!(
            (range.from, range.to),
            (date("2026-09-03"), date("2026-09-10"))
        );
        let zone = TimeZone::fixed(jiff::tz::offset(2));
        assert_eq!(
            range.bounds(&zone),
            Some((at("2026-09-02T22:00:00Z"), at("2026-09-10T22:00:00Z"))),
            "local midnights, the last day whole"
        );
        assert_eq!(
            DateRange::ending(date("2026-09-10"), 7),
            DateRange::new(date("2026-09-04"), date("2026-09-10"))
        );
        assert_eq!(
            ActiveWithin::Between(range).window(&TimeZone::UTC),
            SessionWindow::between(at("2026-09-03T00:00:00Z"), at("2026-09-11T00:00:00Z"))
        );
        assert_eq!(
            ActiveWithin::Week.window(&TimeZone::UTC),
            SessionWindow::recent(Some(7))
        );
    }

    #[test]
    fn a_range_names_its_dates_and_their_year_only_when_another() {
        let today = date("2026-11-01");
        let this_year = DateRange::new(date("2026-09-03"), date("2026-09-10"));
        let last_year = DateRange::new(date("2025-12-30"), date("2026-01-02"));
        let one_day = DateRange::new(date("2026-09-03"), date("2026-09-03"));
        assert!(
            !this_year.label(today).contains("2026"),
            "{}",
            this_year.label(today)
        );
        assert!(
            last_year.label(today).contains("2025"),
            "{}",
            last_year.label(today)
        );
        assert_ne!(one_day.label(today), this_year.label(today));
        assert!(this_year.label(today).starts_with(&one_day.label(today)));
    }

    #[test]
    fn a_range_keeps_what_is_in_it_and_what_is_listed_first() {
        let bounds = (at("2026-09-03T00:00:00Z"), at("2026-09-11T00:00:00Z"));
        let mut session = session("primary", false);
        session.last_activity = at("2026-09-05T12:00:00Z");
        assert!(in_range(&session, bounds));
        session.last_activity = at("2026-09-11T00:00:00Z");
        assert!(
            !in_range(&session, bounds),
            "the end is the next day's start"
        );
        session.pinned = true;
        assert!(in_range(&session, bounds));
        session.pinned = false;
        session.unread = true;
        assert!(in_range(&session, bounds));
    }

    #[test]
    fn the_windows_reach_back_further_in_order() {
        let days = ActiveWithin::ALL.map(|within| within.days().unwrap_or(i64::MAX));
        assert!(days.is_sorted());
        let mut ids = ActiveWithin::ALL.map(ActiveWithin::id).to_vec();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), ActiveWithin::ALL.len());
    }

    #[test]
    fn agents_and_machines_each_narrow_and_any_of_them_passes() {
        let claude = session("claude", false);
        let primary = session("primary", false);
        let mut filter = SessionFilter::default();
        assert!(filter.by_date_only());
        assert!(filter.matches(&claude, &[]), "nothing chosen passes all");

        filter.toggle_agent("claude");
        assert!(filter.matches(&claude, &[]));
        assert!(!filter.matches(&primary, &[]));
        filter.toggle_agent("primary");
        assert!(filter.matches(&primary, &[]), "either agent passes");

        filter.toggle_machine("labpc");
        assert!(filter.matches(&claude, &names(&["laptop", "labpc"])));
        assert!(!filter.matches(&claude, &names(&["laptop"])));
        assert!(!filter.matches(&claude, &[]), "no machine is not labpc");
        assert_eq!(filter.changed(), 2);

        filter.toggle_machine("labpc");
        filter.toggle_agent("claude");
        filter.toggle_agent("primary");
        filter.within = ActiveWithin::Ever;
        assert!(filter.by_date_only());
        assert_eq!(filter.changed(), 1);
    }

    #[test]
    fn the_options_hold_what_is_listed_and_what_is_chosen() {
        let sessions = [
            session("primary", false),
            session("claude", false),
            session("primary", false),
            session("subagent-only", true),
        ];
        let chosen = BTreeSet::from(["codex".to_owned()]);
        assert_eq!(
            agent_options(&sessions, &chosen),
            names(&["claude", "codex", "primary"])
        );
        assert_eq!(
            machine_options(
                [names(&["labpc", "laptop"]), names(&["labpc"])],
                &BTreeSet::from(["gone".to_owned()]),
            ),
            names(&["gone", "labpc", "laptop"])
        );
    }
}
