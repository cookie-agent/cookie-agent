//! Unread root sessions (`docs/site/specs/unread-sessions.md`): the daemon
//! keeps them; clients show `SessionMeta::unread` and mark sessions with
//! `session.unread.set`.

use cookie_agent_protocol::SemVer;

/// The first client protocol whose daemons keep unread sessions.
pub const UNREAD_SINCE: SemVer = SemVer::new(1, 51, 0);

/// Whether the daemon, speaking `version`, keeps unread sessions.
#[must_use]
pub fn daemon_keeps_unread(version: Option<SemVer>) -> bool {
    version.is_some_and(|version| version >= UNREAD_SINCE)
}

/// What a status line says after marking `count` sessions `unread` or read.
#[must_use]
pub fn marked_message(count: usize, unread: bool) -> String {
    let state = if unread { "unread" } else { "read" };
    match count {
        0 => format!("Nothing changed: already {state}."),
        1 => format!("Marked 1 session {state}."),
        count => format!("Marked {count} sessions {state}."),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unread_needs_protocol_1_51() {
        assert!(daemon_keeps_unread(Some(SemVer::new(1, 51, 0))));
        assert!(!daemon_keeps_unread(Some(SemVer::new(1, 50, 0))));
        assert!(!daemon_keeps_unread(None));
    }

    #[test]
    fn the_marked_message_counts_what_changed() {
        assert_eq!(marked_message(0, false), "Nothing changed: already read.");
        assert_eq!(marked_message(1, true), "Marked 1 session unread.");
        assert_eq!(marked_message(3, false), "Marked 3 sessions read.");
    }
}
