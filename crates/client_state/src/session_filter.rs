//! The session list's filter (`docs/site/specs/session-pins-and-filters.md`):
//! how far back the list reaches, and which agents and machines its
//! sessions run on. The date is the list's window
//! ([`crate::session_window`]), which the daemon answers with every pinned,
//! unread, and busy session too; the agents and the machines narrow what
//! the list holds, those sessions included.

use std::collections::BTreeSet;

use cookie_agent_protocol::{SessionMeta, SessionOrigin};

/// How far back the list reaches.
#[derive(Clone, Copy, Debug, Default, Eq, Hash, PartialEq)]
pub enum ActiveWithin {
    Day,
    #[default]
    Week,
    Month,
    Quarter,
    /// Every session.
    Ever,
}

impl ActiveWithin {
    /// The order the filter offers them in.
    pub const ALL: [Self; 5] = [
        Self::Day,
        Self::Week,
        Self::Month,
        Self::Quarter,
        Self::Ever,
    ];

    /// The days it reaches back; `None`: every session.
    #[must_use]
    pub const fn days(self) -> Option<i64> {
        match self {
            Self::Day => Some(1),
            Self::Week => Some(7),
            Self::Month => Some(30),
            Self::Quarter => Some(90),
            Self::Ever => None,
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
        }
    }

    /// What the list's summary says of it.
    #[must_use]
    pub const fn summary(self) -> &'static str {
        match self {
            Self::Day => "Last 24 hours",
            Self::Week => "Last 7 days",
            Self::Month => "Last 30 days",
            Self::Quarter => "Last 90 days",
            Self::Ever => "All time",
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
        }
    }
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
