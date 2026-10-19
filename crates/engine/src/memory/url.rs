//! Memory URLs (`docs/site/specs/memory.md` section 5) and the names they
//! are made of (section 4.2).
//!
//! Parsing is strict: other schemes, missing or extra segments, `.md`
//! suffixes, and upper-case letters are errors, never normalized. Every
//! valid URL maps to exactly one file inside the memory folder.

use std::{
    fmt,
    path::{Path, PathBuf},
    str::FromStr,
};

use cookie_agent_identity::AgentId;
use cookie_agent_protocol::MachineName;

use super::{AGENTS_DIR, MACHINES_DIR, MEMORY_EXTENSION, PROJECTS_DIR, USER_DIR};

/// What a memory is about.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub(crate) enum MemoryLayer {
    User,
    Agent,
    Machine,
    Project,
}

impl MemoryLayer {
    pub(crate) const ALL: [Self; 4] = [Self::User, Self::Agent, Self::Machine, Self::Project];

    /// The layer's name, which is also its URL scheme.
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::User => "user",
            Self::Agent => "agent",
            Self::Machine => "machine",
            Self::Project => "project",
        }
    }

    /// The layer named `value`, as the `layer` argument of a search spells
    /// it.
    pub(crate) fn parse(value: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|layer| layer.as_str() == value)
    }

    /// The layer's folder inside the memory folder.
    pub(crate) const fn dir(self) -> &'static str {
        match self {
            Self::User => USER_DIR,
            Self::Agent => AGENTS_DIR,
            Self::Machine => MACHINES_DIR,
            Self::Project => PROJECTS_DIR,
        }
    }
}

impl fmt::Display for MemoryLayer {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// A category, memory name, or project name: a lowercase ASCII letter or
/// digit, then up to 63 lowercase letters, digits, or `-`.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub(crate) struct MemoryName(String);

impl MemoryName {
    pub(crate) const MAX_BYTES: usize = 64;

    pub(crate) fn new(value: impl Into<String>) -> Option<Self> {
        let value = value.into();
        valid_name(&value).then_some(Self(value))
    }

    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for MemoryName {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

fn valid_name(value: &str) -> bool {
    (1..=MemoryName::MAX_BYTES).contains(&value.len())
        && value
            .as_bytes()
            .first()
            .is_some_and(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit())
        && value
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
}

/// The agent segment that only permission patterns know: a memory URL
/// always names the real agent ID.
const SELF_AGENT: &str = "self";

/// The agent of an `agent://` URL: an agent ID other than `self`.
fn agent_segment(value: &str) -> Option<AgentId> {
    if value == SELF_AGENT {
        return None;
    }
    AgentId::new(value).ok()
}

/// One memory's address.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub(crate) enum MemoryUrl {
    /// `user://<category>`
    User { category: MemoryName },
    /// `agent://<agent>`
    Agent { agent: AgentId },
    /// `machine://<machine>/<name>`
    Machine {
        machine: MachineName,
        name: MemoryName,
    },
    /// `project://<project>/<name>`
    Project {
        project: MemoryName,
        name: MemoryName,
    },
}

/// Why a string is not a memory URL.
#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
pub(crate) enum MemoryUrlError {
    #[error(
        "`{0}` is not a memory URL; use user://<category>, agent://<agent>, machine://<machine>/<name>, or project://<project>/<name>"
    )]
    Scheme(String),
    #[error("`{url}` must have the form {form}")]
    Segments { url: String, form: &'static str },
    #[error("`{segment}` in `{url}` is not a valid {what}")]
    Segment {
        url: String,
        segment: String,
        what: &'static str,
    },
}

const NAME_RULE: &str =
    "name (a lowercase letter or digit, then up to 63 lowercase letters, digits, or `-`)";
const AGENT_RULE: &str = "agent ID";
const MACHINE_RULE: &str = "machine name";

impl MemoryUrl {
    pub(crate) fn parse(value: &str) -> Result<Self, MemoryUrlError> {
        let (scheme, rest) = value
            .split_once("://")
            .ok_or_else(|| MemoryUrlError::Scheme(value.to_owned()))?;
        let layer =
            MemoryLayer::parse(scheme).ok_or_else(|| MemoryUrlError::Scheme(value.to_owned()))?;
        let segments = rest.split('/').collect::<Vec<_>>();
        let invalid = |segment: &str, what: &'static str| MemoryUrlError::Segment {
            url: value.to_owned(),
            segment: segment.to_owned(),
            what,
        };
        let name =
            |segment: &str| MemoryName::new(segment).ok_or_else(|| invalid(segment, NAME_RULE));
        let form = |form: &'static str| MemoryUrlError::Segments {
            url: value.to_owned(),
            form,
        };
        match (layer, segments.as_slice()) {
            (MemoryLayer::User, [category]) => Ok(Self::User {
                category: name(category)?,
            }),
            (MemoryLayer::User, _) => Err(form("user://<category>")),
            (MemoryLayer::Agent, [agent]) => Ok(Self::Agent {
                agent: agent_segment(agent).ok_or_else(|| invalid(agent, AGENT_RULE))?,
            }),
            (MemoryLayer::Agent, _) => Err(form("agent://<agent>")),
            (MemoryLayer::Machine, [machine, memory]) => Ok(Self::Machine {
                machine: MachineName::new(*machine).map_err(|_| invalid(machine, MACHINE_RULE))?,
                name: name(memory)?,
            }),
            (MemoryLayer::Machine, _) => Err(form("machine://<machine>/<name>")),
            (MemoryLayer::Project, [project, memory]) => Ok(Self::Project {
                project: name(project)?,
                name: name(memory)?,
            }),
            (MemoryLayer::Project, _) => Err(form("project://<project>/<name>")),
        }
    }

    pub(crate) const fn layer(&self) -> MemoryLayer {
        match self {
            Self::User { .. } => MemoryLayer::User,
            Self::Agent { .. } => MemoryLayer::Agent,
            Self::Machine { .. } => MemoryLayer::Machine,
            Self::Project { .. } => MemoryLayer::Project,
        }
    }

    /// The memory's own name: the category, the agent ID, or the name after
    /// the machine or project.
    pub(crate) fn name(&self) -> &str {
        match self {
            Self::User { category } => category.as_str(),
            Self::Agent { agent } => agent.as_str(),
            Self::Machine { name, .. } | Self::Project { name, .. } => name.as_str(),
        }
    }

    /// The machine or project a memory belongs to, if any.
    pub(crate) fn group(&self) -> Option<&str> {
        match self {
            Self::User { .. } | Self::Agent { .. } => None,
            Self::Machine { machine, .. } => Some(machine.as_str()),
            Self::Project { project, .. } => Some(project.as_str()),
        }
    }

    /// The folder holding the memory, relative to the memory folder.
    pub(crate) fn relative_dir(&self) -> PathBuf {
        let layer = Path::new(self.layer().dir());
        match self.group() {
            Some(group) => layer.join(group),
            None => layer.to_owned(),
        }
    }

    /// The memory's file, relative to the memory folder.
    pub(crate) fn relative_path(&self) -> PathBuf {
        self.relative_dir()
            .join(format!("{}.{MEMORY_EXTENSION}", self.name()))
    }
}

impl fmt::Display for MemoryUrl {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}://", self.layer())?;
        if let Some(group) = self.group() {
            write!(formatter, "{group}/")?;
        }
        formatter.write_str(self.name())
    }
}

impl FromStr for MemoryUrl {
    type Err = MemoryUrlError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::parse(value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn valid_urls_round_trip_and_map_inside_the_memory_folder() {
        for (url, path) in [
            ("user://preferences", "user/preferences.md"),
            ("user://0", "user/0.md"),
            ("agent://default", "agents/default.md"),
            ("agent://code-reviewer", "agents/code-reviewer.md"),
            ("machine://labpc/read-only", "machines/labpc/read-only.md"),
            ("machine://lab_pc-2/gpu", "machines/lab_pc-2/gpu.md"),
            (
                "project://cookie-agent/build",
                "projects/cookie-agent/build.md",
            ),
            ("project://9lives/a-", "projects/9lives/a-.md"),
        ] {
            let parsed = MemoryUrl::parse(url).unwrap_or_else(|error| panic!("{url}: {error}"));
            assert_eq!(parsed.to_string(), url);
            assert_eq!(parsed.relative_path(), Path::new(path), "{url}");
        }
    }

    #[test]
    fn names_are_one_to_sixty_four_bytes() {
        let longest = format!("a{}", "-".repeat(63));
        assert!(MemoryName::new(longest.clone()).is_some());
        assert!(MemoryName::new(format!("{longest}x")).is_none());
        assert!(MemoryName::new("").is_none());
        assert!(MemoryName::new("-a").is_none());
        assert!(MemoryName::new("a_b").is_none());
        assert!(MemoryName::new("a.b").is_none());
        let url = format!("project://{longest}/{longest}");
        assert!(MemoryUrl::parse(&url).is_ok());
    }

    #[test]
    fn malformed_urls_are_errors_not_normalized() {
        for url in [
            "",
            "preferences",
            "user:preferences",
            "user:/preferences",
            "users://preferences",
            "USER://preferences",
            "file://user/preferences",
            "user://",
            "user://Preferences",
            "user://preferences.md",
            "user://preferences/",
            "user://a/b",
            "user://../agents/default",
            "user://.",
            "user://pre ferences",
            "agent://self",
            "agent://Default",
            "agent://default/x",
            "agent://a--b",
            "agent://a-",
            "agent://",
            "machine://labpc",
            "machine://labpc/",
            "machine://Labpc/gpu",
            "machine://1pc/gpu",
            "machine://labpc/gpu/x",
            "machine://labpc/gpu.md",
            "machine:///gpu",
            "project://x",
            "project://x/y/z",
            "project://../y",
            "project://x/../y",
            "project://x_y/z",
            "project://x/y?z",
            "project://x/y#z",
            "project://x//y",
        ] {
            assert!(MemoryUrl::parse(url).is_err(), "{url} parsed");
        }
    }

    #[test]
    fn layers_parse_by_their_names_only() {
        for layer in MemoryLayer::ALL {
            assert_eq!(MemoryLayer::parse(layer.as_str()), Some(layer));
        }
        assert_eq!(MemoryLayer::parse("agents"), None);
        assert_eq!(MemoryLayer::parse("User"), None);
    }
}
