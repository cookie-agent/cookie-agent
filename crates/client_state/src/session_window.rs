//! How far back a client's session list reaches
//! (`docs/site/specs/session-storage-v3.md` section 4.3): the last 30 days
//! at first, then a month further each time the user asks for older
//! sessions. Unread and busy sessions come with every answer whatever
//! their age.

use cookie_agent_protocol::{SemVer, SessionListParams, SessionMeta};
use jiff::{SignedDuration, Timestamp};

/// The first client protocol whose daemons list sessions by activity.
pub const LIST_WINDOW_SINCE: SemVer = SemVer::new(1, 57, 0);

/// How many days a session list shows before the user asks for more.
pub const RECENT_DAYS: i64 = 30;

/// Whether the daemon, speaking `version`, takes `session.list`'s window.
#[must_use]
pub fn daemon_lists_by_activity(version: Option<SemVer>) -> bool {
    version.is_some_and(|version| version >= LIST_WINDOW_SINCE)
}

/// How far back the list reaches.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
enum Reach {
    /// The last [`RECENT_DAYS`] days, counted when the list is read.
    #[default]
    Recent,
    /// Back to this time, after older pages came in.
    Since(Timestamp),
    /// Every session.
    All,
}

/// The session list's window, and whether the daemon has older sessions.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct SessionWindow {
    reach: Reach,
    /// The `older` cursor of the newest answer.
    older: Option<Timestamp>,
}

impl SessionWindow {
    /// `params` with the window a full read of the list asks for at `now`.
    /// A daemon older than [`LIST_WINDOW_SINCE`] gets none, and lists
    /// everything.
    #[must_use]
    pub fn read_params(
        &self,
        mut params: SessionListParams,
        version: Option<SemVer>,
        now: Timestamp,
    ) -> SessionListParams {
        if daemon_lists_by_activity(version) {
            params.active_since = match self.reach {
                Reach::Recent => now
                    .checked_sub(SignedDuration::from_hours(24 * RECENT_DAYS))
                    .ok(),
                Reach::Since(since) => Some(since),
                Reach::All => None,
            };
        }
        params
    }

    /// Records a full read's `older` cursor.
    pub fn read(&mut self, older: Option<Timestamp>) {
        self.older = older;
    }

    /// The parameters of the next older page, when the daemon has one.
    #[must_use]
    pub fn older_params(&self, mut params: SessionListParams) -> Option<SessionListParams> {
        params.before = Some(self.older?);
        params.active_since = None;
        Some(params)
    }

    /// Whether the daemon has sessions older than the list shows.
    #[must_use]
    pub fn has_older(&self) -> bool {
        self.older.is_some()
    }

    /// Records an older page's `older` cursor: the list now reaches back to
    /// it, or to the first session once nothing is older.
    pub fn paged(&mut self, older: Option<Timestamp>) {
        self.reach = older.map_or(Reach::All, Reach::Since);
        self.older = older;
    }

    /// Back to the last [`RECENT_DAYS`] days, as for another daemon.
    pub fn reset(&mut self) {
        *self = Self::default();
    }
}

/// Adds the sessions of an older page to `sessions`, keeping the copy the
/// list already holds of any session in both: it may be newer.
pub fn merge_older(sessions: &mut Vec<SessionMeta>, page: Vec<SessionMeta>) {
    for session in page {
        if !sessions
            .iter()
            .any(|listed| listed.session_id == session.session_id)
        {
            sessions.push(session);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(text: &str) -> Timestamp {
        text.parse().expect("test time")
    }

    const WINDOWED: Option<SemVer> = Some(LIST_WINDOW_SINCE);

    #[test]
    fn a_daemon_before_the_window_lists_everything() {
        let window = SessionWindow::default();
        let params = window.read_params(
            SessionListParams::default(),
            Some(SemVer::new(1, 56, 0)),
            at("2026-10-31T00:00:00Z"),
        );
        assert_eq!(params, SessionListParams::default());
        assert!(!daemon_lists_by_activity(None));
    }

    #[test]
    fn the_list_reaches_back_a_page_at_a_time_and_then_to_everything() {
        let now = at("2026-10-31T12:00:00Z");
        let mut window = SessionWindow::default();
        assert_eq!(
            window
                .read_params(SessionListParams::default(), WINDOWED, now)
                .active_since,
            Some(at("2026-10-01T12:00:00Z")),
            "thirty days of twenty-four hours"
        );
        assert_eq!(window.older_params(SessionListParams::default()), None);

        window.read(Some(at("2026-10-01T12:00:00Z")));
        assert!(window.has_older());
        let page = window
            .older_params(SessionListParams::default())
            .expect("an older page");
        assert_eq!(page.before, Some(at("2026-10-01T12:00:00Z")));
        assert_eq!(page.active_since, None);

        // The page reached back to September's start: a later read keeps
        // what it showed.
        window.paged(Some(at("2026-09-01T00:00:00Z")));
        assert_eq!(
            window
                .read_params(SessionListParams::default(), WINDOWED, now)
                .active_since,
            Some(at("2026-09-01T00:00:00Z"))
        );

        // Nothing is older: every later read lists everything.
        window.paged(None);
        assert!(!window.has_older());
        let params = window.read_params(SessionListParams::default(), WINDOWED, now);
        assert_eq!(params.active_since, None);
        assert_eq!(params.before, None);

        window.reset();
        assert_eq!(window, SessionWindow::default());
    }

    fn session(title: &str) -> SessionMeta {
        serde_json::from_value(serde_json::json!({
            "session_id": cookie_agent_protocol::SessionId::new_v7(),
            "origin": {"type": "root"},
            "short_id": "test",
            "cwd_identity": "/w",
            "title": title,
            "creation_selection": {
                "agent": "primary",
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
            "last_activity": "2026-09-01T00:00:00Z",
            "status": "idle",
            "skipped_events": [],
        }))
        .expect("session meta")
    }

    #[test]
    fn an_older_page_keeps_the_lists_own_copy_of_a_session() {
        let listed = session("listed now");
        let mut stale = listed.clone();
        stale.title = None;
        let older = session("older");
        let mut sessions = vec![listed.clone()];
        merge_older(&mut sessions, vec![stale, older.clone()]);
        assert_eq!(sessions, vec![listed, older]);
    }
}
