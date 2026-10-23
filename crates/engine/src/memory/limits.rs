//! The `[memory]` limits as one value (`docs/site/specs/memory.md` sections
//! 11 and 16.9.7): one size limit per kind of memory document, and the
//! snapshot's room. They are read from the configuration at each
//! operation, so `config.apply` and `config.reload` take effect at once.

use super::{projects::is_overview, url::MemoryUrl};

/// The `[memory]` limits of one operation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct MemoryLimits {
    pub(crate) user_max_bytes: usize,
    pub(crate) agent_max_bytes: usize,
    pub(crate) machine_max_bytes: usize,
    pub(crate) project_max_bytes: usize,
    pub(crate) overview_max_bytes: usize,
    pub(crate) snapshot_user_bytes: usize,
    pub(crate) snapshot_index_bytes: usize,
}

impl MemoryLimits {
    pub(crate) fn of(config: &cookie_agent_config::MemoryConfig) -> Self {
        Self {
            user_max_bytes: config.user_max_bytes,
            agent_max_bytes: config.agent_max_bytes,
            machine_max_bytes: config.machine_max_bytes,
            project_max_bytes: config.project_max_bytes,
            overview_max_bytes: config.overview_max_bytes,
            snapshot_user_bytes: config.snapshot_user_bytes,
            snapshot_index_bytes: config.snapshot_index_bytes,
        }
    }

    /// The largest the memory at `url` may be, frontmatter included.
    pub(crate) fn max_bytes(&self, url: &MemoryUrl) -> usize {
        match url {
            MemoryUrl::User { .. } => self.user_max_bytes,
            MemoryUrl::Agent { .. } => self.agent_max_bytes,
            MemoryUrl::Machine { .. } => self.machine_max_bytes,
            MemoryUrl::Project { .. } if is_overview(url) => self.overview_max_bytes,
            MemoryUrl::Project { .. } => self.project_max_bytes,
        }
    }
}

#[cfg(test)]
impl MemoryLimits {
    /// Every limit at `bytes`.
    pub(crate) const fn all(bytes: usize) -> Self {
        Self {
            user_max_bytes: bytes,
            agent_max_bytes: bytes,
            machine_max_bytes: bytes,
            project_max_bytes: bytes,
            overview_max_bytes: bytes,
            snapshot_user_bytes: bytes,
            snapshot_index_bytes: bytes,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_kind_of_memory_has_its_own_limit() {
        let limits = MemoryLimits {
            user_max_bytes: 1,
            agent_max_bytes: 2,
            machine_max_bytes: 3,
            project_max_bytes: 4,
            overview_max_bytes: 5,
            snapshot_user_bytes: 6,
            snapshot_index_bytes: 7,
        };
        for (url, bytes) in [
            ("user://feedback", 1),
            ("agent://reviewer", 2),
            ("machine://labpc/gpu", 3),
            ("project://web/release", 4),
            ("project://web/overview", 5),
        ] {
            assert_eq!(
                limits.max_bytes(&MemoryUrl::parse(url).unwrap()),
                bytes,
                "{url}"
            );
        }
    }
}
