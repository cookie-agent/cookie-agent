//! What a client tells the daemon it shows the user, so the daemon pushes
//! no notification for it (`docs/site/specs/push-notifications.md` section
//! 8). Every client decides it the same way.

use std::time::Duration;

use cookie_agent_protocol::SemVer;

/// The first client protocol whose daemons push notifications and take
/// `client.presence`.
pub const PUSH_SINCE: SemVer = SemVer::new(1, 30, 0);

/// How long without input before a client says the user is not there.
pub const IDLE_AFTER: Duration = Duration::from_secs(120);

/// Whether the daemon, speaking `version`, pushes notifications.
#[must_use]
pub fn daemon_has_push(version: Option<SemVer>) -> bool {
    version.is_some_and(|version| version >= PUSH_SINCE)
}

/// What a client shows the user: the `viewed` session while its window
/// is `attended` (visible and focused) and was used in the last
/// [`IDLE_AFTER`]; `None` when the user is not there.
#[must_use]
pub fn shown<T>(viewed: Option<T>, attended: bool, idle: Duration) -> Option<T> {
    viewed.filter(|_| attended && idle < IDLE_AFTER)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_session_is_shown_only_to_an_attended_recently_used_client() {
        assert_eq!(shown(Some(1), true, Duration::ZERO), Some(1));
        assert_eq!(shown(Some(1), false, Duration::ZERO), None);
        assert_eq!(shown(Some(1), true, IDLE_AFTER), None);
        assert_eq!(shown(None::<u8>, true, Duration::ZERO), None);
    }

    #[test]
    fn push_needs_protocol_1_30() {
        assert!(daemon_has_push(Some(SemVer::new(1, 30, 0))));
        assert!(!daemon_has_push(Some(SemVer::new(1, 29, 0))));
        assert!(!daemon_has_push(None));
    }
}
