//! The GitHub star card for clients (`docs/site/specs/github-star.md`):
//! whether a daemon has it.

use cookie_agent_protocol::SemVer;

/// The client protocol minor that brought `star.status` and `star.answer`.
const STAR_MINOR: u32 = 106;

/// Whether a daemon of `version` answers `star.status`.
#[must_use]
pub fn daemon_has_star(version: Option<SemVer>) -> bool {
    version.is_some_and(|version| {
        version
            .shared_minor(cookie_agent_protocol::CLIENT_PROTOCOL_VERSION)
            .is_some_and(|minor| minor >= STAR_MINOR)
    })
}
