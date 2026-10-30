//! Which release this build is: its tag, channel, and target, as the
//! release workflow stamps them (`COOKIE_RELEASE_TAG`, `COOKIE_GIT_HASH`).

use std::{fmt, sync::LazyLock};

/// This build's release tag, such as `v0.4.0` or `v0.4.0-alpha.1793047587`;
/// empty for a build that is not a release.
pub const RELEASE_TAG: &str = env!("COOKIE_RELEASE_TAG");

/// The commit a release was built from; empty outside the release workflow.
pub const GIT_HASH: &str = env!("COOKIE_GIT_HASH");

/// The target triple this binary was built for, which names its release
/// archive.
pub const TARGET: &str = env!("COOKIE_TARGET");

/// Where a build comes from, and so which releases it updates to.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Channel {
    /// A `vX.Y.Z` release.
    Stable,
    /// A `vX.Y.Z-<pre>` prerelease: today the daily `-alpha.<unix seconds>`.
    Nightly,
    /// Not a release: `cargo install`, a local build, a test build.
    Source,
}

impl Channel {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Stable => "stable",
            Self::Nightly => "nightly",
            Self::Source => "source",
        }
    }
}

impl fmt::Display for Channel {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// A release tag's version: `v0.4.0-alpha.1793047587` is core `(0, 4, 0)`
/// with prerelease `alpha.1793047587`. Build metadata is dropped.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReleaseVersion {
    pub core: (u64, u64, u64),
    pub pre: String,
}

impl ReleaseVersion {
    /// Parses a tag, with or without its leading `v`; `None` unless it is
    /// `major.minor.patch` with an optional `-<pre>`.
    pub fn parse(tag: &str) -> Option<Self> {
        let version = tag.strip_prefix('v').unwrap_or(tag);
        let version = version.split('+').next()?;
        let (core, pre) = version.split_once('-').unwrap_or((version, ""));
        let mut parts = core.split('.').map(|part| {
            (!part.is_empty() && part.bytes().all(|byte| byte.is_ascii_digit()))
                .then(|| part.parse::<u64>().ok())
                .flatten()
        });
        let core = (parts.next()??, parts.next()??, parts.next()??);
        if parts.next().is_some() || (version.contains('-') && pre.is_empty()) {
            return None;
        }
        Some(Self {
            core,
            pre: pre.to_owned(),
        })
    }

    /// The channel a release of this version is on.
    pub fn channel(&self) -> Channel {
        if self.pre.is_empty() {
            Channel::Stable
        } else {
            Channel::Nightly
        }
    }

    /// A nightly's build time, which orders nightlies: `alpha.<n>` is `n`.
    /// Their semver order does not, since `main` keeps the last released
    /// version and `v0.4.0-alpha.<n>` built after `v0.4.0` sorts below it.
    pub fn nightly_stamp(&self) -> Option<u64> {
        self.pre.strip_prefix("alpha.")?.parse().ok()
    }
}

/// The channel of the release tagged `tag`, [`Channel::Source`] when `tag`
/// is empty or not a release tag.
pub fn channel_of(tag: &str) -> Channel {
    ReleaseVersion::parse(tag).map_or(Channel::Source, |version| version.channel())
}

/// This build's channel.
pub fn channel() -> Channel {
    channel_of(RELEASE_TAG)
}

/// This build's version: its release tag without the `v`, or the package
/// version for a build that is not a release. What `cookie --version`
/// starts with and what the daemon reports to clients.
pub fn version() -> &'static str {
    match channel() {
        Channel::Source => env!("CARGO_PKG_VERSION"),
        Channel::Stable | Channel::Nightly => RELEASE_TAG.strip_prefix('v').unwrap_or(RELEASE_TAG),
    }
}

/// `cookie --version`'s text after `cookie`: the version, the channel, and
/// the commit's first nine characters when known, as in
/// `0.4.0-alpha.1793047587 (nightly, 67b3d8132)`.
pub fn long_version() -> &'static str {
    static LONG: LazyLock<String> = LazyLock::new(|| {
        let commit = GIT_HASH.get(..9).unwrap_or(GIT_HASH);
        if commit.is_empty() {
            format!("{} ({})", version(), channel())
        } else {
            format!("{} ({}, {commit})", version(), channel())
        }
    });
    LONG.as_str()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tags_name_their_channel() {
        assert_eq!(channel_of("v0.4.0"), Channel::Stable);
        assert_eq!(channel_of("0.4.0"), Channel::Stable);
        assert_eq!(channel_of("v0.4.0-alpha.1793047587"), Channel::Nightly);
        assert_eq!(channel_of("v1.0.0-rc.1"), Channel::Nightly);
        assert_eq!(channel_of(""), Channel::Source);
        assert_eq!(channel_of("v0.4"), Channel::Source);
        assert_eq!(channel_of("v0.4.0.1"), Channel::Source);
        assert_eq!(channel_of("v0.4.0-"), Channel::Source);
        assert_eq!(channel_of("nightly"), Channel::Source);
        assert_eq!(channel_of("v+1.0.0"), Channel::Source);
    }

    #[test]
    fn a_tag_parses_into_core_and_prerelease() {
        assert_eq!(
            ReleaseVersion::parse("v0.4.0-alpha.1793047587+abc"),
            Some(ReleaseVersion {
                core: (0, 4, 0),
                pre: "alpha.1793047587".to_owned(),
            })
        );
        assert_eq!(
            ReleaseVersion::parse("v12.0.3").map(|version| version.core),
            Some((12, 0, 3))
        );
    }

    #[test]
    fn only_alpha_prereleases_carry_a_nightly_stamp() {
        let stamp = |tag| ReleaseVersion::parse(tag).and_then(|version| version.nightly_stamp());
        assert_eq!(stamp("v0.4.0-alpha.1793047587"), Some(1_793_047_587));
        assert_eq!(stamp("v0.4.0-beta.3"), None);
        assert_eq!(stamp("v0.4.0-alpha.x"), None);
        assert_eq!(stamp("v0.4.0"), None);
    }
}
