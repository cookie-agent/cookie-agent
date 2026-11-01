//! Pinned root sessions (`docs/site/specs/session-pins-and-filters.md`):
//! the daemon keeps them; clients show `SessionMeta::pinned`, list them
//! with the sessions at work, and pin with `session.pin.set`.

use cookie_agent_protocol::SemVer;

/// The first client protocol whose daemons keep pinned sessions.
pub const PINS_SINCE: SemVer = SemVer::new(1, 66, 0);

/// Whether the daemon, speaking `version`, keeps pinned sessions.
#[must_use]
pub fn daemon_keeps_pins(version: Option<SemVer>) -> bool {
    version.is_some_and(|version| version >= PINS_SINCE)
}

/// What a status line says after pinning or unpinning `count` sessions.
#[must_use]
pub fn pinned_message(count: usize, pinned: bool) -> String {
    let (state, done) = if pinned {
        ("pinned", "Pinned")
    } else {
        ("unpinned", "Unpinned")
    };
    match count {
        0 => format!("Nothing changed: already {state}."),
        1 => format!("{done} 1 session."),
        count => format!("{done} {count} sessions."),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pins_need_protocol_1_66() {
        assert!(daemon_keeps_pins(Some(PINS_SINCE)));
        assert!(!daemon_keeps_pins(Some(SemVer::new(1, 65, 0))));
        assert!(!daemon_keeps_pins(None));
    }
}
