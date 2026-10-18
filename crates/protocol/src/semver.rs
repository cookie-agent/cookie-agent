//! Semver protocol versions, as specified in
//! `docs/site/specs/compatibility-v1.md` section 3.
//!
//! Each surface's version is the string `"MAJOR.MINOR.PATCH"`: three decimal
//! integers without leading zeros, and no pre-release or build suffix. Peers
//! with different majors do not connect; peers with the same major share the
//! lower of their two minors. Before the freeze (major 0) every minor may
//! break the wire, as semver's `0.x` convention allows, so peers must also
//! agree on the minor.
//!
//! The plugin and runtime handshakes send [`PLUGIN_PROTOCOL_VERSION`] and
//! [`RUNTIME_PROTOCOL_VERSION`] and read the peer's with
//! [`negotiate_version`]; the client handshake still sends the exact numeric
//! [`crate::ProtocolVersion`] until it switches. The constants stay `0.x`
//! until the freeze.

use std::{borrow::Cow, fmt, str::FromStr};

use schemars::{JsonSchema, Schema, SchemaGenerator, json_schema};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// The client protocol version this build speaks.
pub const CLIENT_PROTOCOL_VERSION: SemVer = SemVer::new(0, 50, 0);
/// The plugin protocol version this build speaks.
pub const PLUGIN_PROTOCOL_VERSION: SemVer = SemVer::new(0, 4, 0);
/// The runtime protocol version this build speaks.
pub const RUNTIME_PROTOCOL_VERSION: SemVer = SemVer::new(0, 7, 0);

/// The handshake member that carries a peer's version.
pub const PROTOCOL_VERSION_MEMBER: &str = "protocol_version";

/// A `MAJOR.MINOR.PATCH` protocol version.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct SemVer {
    major: u32,
    minor: u32,
    patch: u32,
}

impl SemVer {
    #[must_use]
    pub const fn new(major: u32, minor: u32, patch: u32) -> Self {
        Self {
            major,
            minor,
            patch,
        }
    }

    /// Parses `"MAJOR.MINOR.PATCH"`.
    pub fn parse(text: &str) -> Result<Self, SemVerError> {
        let mut parts = text.split('.');
        let mut next = || parts.next().ok_or(SemVerError::Malformed).and_then(part);
        let version = Self::new(next()?, next()?, next()?);
        if parts.next().is_some() {
            return Err(SemVerError::Malformed);
        }
        Ok(version)
    }

    #[must_use]
    pub const fn major(self) -> u32 {
        self.major
    }

    #[must_use]
    pub const fn minor(self) -> u32 {
        self.minor
    }

    #[must_use]
    pub const fn patch(self) -> u32 {
        self.patch
    }

    /// Whether a peer at `other` may connect: the majors are equal and,
    /// while the major is 0, so are the minors.
    #[must_use]
    pub const fn is_compatible_with(self, other: Self) -> bool {
        self.major == other.major && (self.major != 0 || self.minor == other.minor)
    }

    /// The minor two compatible peers share: the lower of the two. `None`
    /// when they are not compatible.
    #[must_use]
    pub fn shared_minor(self, other: Self) -> Option<u32> {
        self.is_compatible_with(other)
            .then(|| self.minor.min(other.minor))
    }
}

fn part(text: &str) -> Result<u32, SemVerError> {
    let digits = !text.is_empty() && text.bytes().all(|byte| byte.is_ascii_digit());
    if !digits || (text.len() > 1 && text.starts_with('0')) {
        return Err(SemVerError::Malformed);
    }
    text.parse().map_err(|_| SemVerError::Malformed)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
pub enum SemVerError {
    #[error("a protocol version is MAJOR.MINOR.PATCH without leading zeros or suffixes")]
    Malformed,
}

impl fmt::Display for SemVer {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}.{}.{}", self.major, self.minor, self.patch)
    }
}

impl FromStr for SemVer {
    type Err = SemVerError;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        Self::parse(text)
    }
}

impl Serialize for SemVer {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for SemVer {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let text = Cow::<str>::deserialize(deserializer)?;
        Self::parse(&text).map_err(serde::de::Error::custom)
    }
}

impl JsonSchema for SemVer {
    fn inline_schema() -> bool {
        true
    }

    fn schema_name() -> Cow<'static, str> {
        Cow::Borrowed("SemVer")
    }

    fn json_schema(_generator: &mut SchemaGenerator) -> Schema {
        json_schema!({
            "type": "string",
            "pattern": "^(0|[1-9][0-9]*)\\.(0|[1-9][0-9]*)\\.(0|[1-9][0-9]*)$",
            "description": "A MAJOR.MINOR.PATCH protocol version."
        })
    }
}

/// What a handshake's raw `protocol_version` member holds.
#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
pub enum PeerVersionError {
    #[error("the handshake has no protocol_version")]
    Missing,
    /// An integer: a build from before semver versions.
    #[error("the peer speaks the old exact protocol version {0}")]
    Legacy(u64),
    #[error("the handshake's protocol_version is not a MAJOR.MINOR.PATCH string")]
    Malformed,
}

/// Reads `protocol_version` from a handshake's raw params before anything
/// else is decoded, so that a peer of another major is answered with
/// `unsupported_protocol` rather than with a decoding error of a shape it
/// does not share.
pub fn peer_protocol_version(params: &Value) -> Result<SemVer, PeerVersionError> {
    match params.get(PROTOCOL_VERSION_MEMBER) {
        None => Err(PeerVersionError::Missing),
        Some(Value::String(text)) => SemVer::parse(text).map_err(|_| PeerVersionError::Malformed),
        Some(Value::Number(number)) => number
            .as_u64()
            .map_or(Err(PeerVersionError::Malformed), |legacy| {
                Err(PeerVersionError::Legacy(legacy))
            }),
        Some(_) => Err(PeerVersionError::Malformed),
    }
}

/// Why a handshake's peer version is refused.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum VersionRefusal {
    /// A version this build cannot speak with: another major, another minor
    /// before 1.0, or the integer version of a build from before semver.
    Unsupported { supported: SemVer, peer: String },
    /// The handshake carries no readable version.
    Malformed(PeerVersionError),
}

impl VersionRefusal {
    /// The answer to the handshake: the registry's `unsupported_protocol`
    /// naming both versions, or `invalid_params`.
    #[must_use]
    pub fn to_rpc_error(&self) -> crate::JsonRpcError {
        match self {
            Self::Unsupported { supported, peer } => crate::errors::PROTOCOL_UNSUPPORTED
                .error()
                .field("supported_version", supported.to_string())
                .field("peer_version", peer)
                .build(),
            Self::Malformed(error) => crate::errors::BAD_PARAMS
                .error()
                .field("field", PROTOCOL_VERSION_MEMBER)
                .detail(error.to_string())
                .build(),
        }
    }
}

impl fmt::Display for VersionRefusal {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unsupported { .. } => formatter.write_str(&self.to_rpc_error().message),
            Self::Malformed(error) => write!(formatter, "the handshake is malformed: {error}"),
        }
    }
}

impl std::error::Error for VersionRefusal {}

/// Reads the peer's version from a handshake's raw params or reply, before
/// anything else of it is decoded, and checks that a build speaking
/// `supported` can talk to it. Returns the peer's version.
pub fn negotiate_version(supported: SemVer, handshake: &Value) -> Result<SemVer, VersionRefusal> {
    match peer_protocol_version(handshake) {
        Ok(peer) if supported.is_compatible_with(peer) => Ok(peer),
        Ok(peer) => Err(VersionRefusal::Unsupported {
            supported,
            peer: peer.to_string(),
        }),
        Err(PeerVersionError::Legacy(peer)) => Err(VersionRefusal::Unsupported {
            supported,
            peer: peer.to_string(),
        }),
        Err(error) => Err(VersionRefusal::Malformed(error)),
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn versions_parse_and_print_as_three_parts() {
        let version = SemVer::parse("1.20.3").unwrap();
        assert_eq!(
            (version.major(), version.minor(), version.patch()),
            (1, 20, 3)
        );
        assert_eq!(version.to_string(), "1.20.3");
        assert_eq!("0.0.0".parse::<SemVer>().unwrap(), SemVer::new(0, 0, 0));
        assert_eq!(
            SemVer::parse("4294967295.0.10").unwrap(),
            SemVer::new(u32::MAX, 0, 10)
        );
    }

    #[test]
    fn malformed_versions_are_rejected() {
        for text in [
            "",
            "1",
            "1.2",
            "1.2.3.4",
            "01.2.3",
            "1.02.3",
            "1.2.03",
            "1.2.3-rc.1",
            "1.2.3+build",
            "v1.2.3",
            "1..3",
            " 1.2.3",
            "1.2.-3",
            "1.2.+3",
            "4294967296.0.0",
            "١.2.3",
        ] {
            assert_eq!(SemVer::parse(text), Err(SemVerError::Malformed), "{text:?}");
        }
    }

    #[test]
    fn versions_order_by_major_then_minor_then_patch() {
        let ordered = ["0.9.9", "1.0.0", "1.0.1", "1.2.0", "1.10.0", "2.0.0"]
            .map(|text| SemVer::parse(text).unwrap());
        assert!(ordered.windows(2).all(|pair| pair[0] < pair[1]));
    }

    #[test]
    fn only_equal_majors_are_compatible_and_share_the_lower_minor() {
        let daemon = SemVer::new(1, 4, 0);
        assert!(daemon.is_compatible_with(SemVer::new(1, 0, 7)));
        assert_eq!(daemon.shared_minor(SemVer::new(1, 2, 9)), Some(2));
        assert_eq!(daemon.shared_minor(SemVer::new(1, 9, 0)), Some(4));
        assert!(!daemon.is_compatible_with(SemVer::new(2, 4, 0)));
        assert!(!daemon.is_compatible_with(SemVer::new(0, 4, 0)));
        assert_eq!(daemon.shared_minor(SemVer::new(2, 0, 0)), None);
    }

    #[test]
    fn before_the_freeze_peers_must_also_agree_on_the_minor() {
        let daemon = SemVer::new(0, 7, 0);
        assert!(daemon.is_compatible_with(SemVer::new(0, 7, 3)));
        assert_eq!(daemon.shared_minor(SemVer::new(0, 7, 3)), Some(7));
        assert!(!daemon.is_compatible_with(SemVer::new(0, 6, 0)));
        assert!(!daemon.is_compatible_with(SemVer::new(0, 8, 0)));
        assert_eq!(daemon.shared_minor(SemVer::new(0, 8, 0)), None);
        assert!(!daemon.is_compatible_with(SemVer::new(1, 7, 0)));
    }

    #[test]
    fn a_handshake_of_another_version_is_refused_with_both_versions() {
        let supported = SemVer::new(0, 3, 0);
        assert_eq!(
            negotiate_version(supported, &json!({"protocol_version": "0.3.9", "x": 1})),
            Ok(SemVer::new(0, 3, 9))
        );
        for (peer, named) in [
            (json!("0.4.0"), "0.4.0"),
            (json!("1.3.0"), "1.3.0"),
            (json!(2), "2"),
        ] {
            let refusal =
                negotiate_version(supported, &json!({"protocol_version": peer})).unwrap_err();
            let error = refusal.to_rpc_error();
            assert_eq!(error.code, crate::errors::UNSUPPORTED_PROTOCOL);
            let data = error.data.unwrap();
            assert_eq!(data["code"], "unsupported_protocol");
            assert_eq!(data["supported_version"], "0.3.0");
            assert_eq!(data["peer_version"], named);
            assert!(
                refusal
                    .to_string()
                    .contains(&format!("0.3.0 here and {named}"))
            );
        }
        for malformed in [json!({}), json!({"protocol_version": "3"})] {
            let refusal = negotiate_version(supported, &malformed).unwrap_err();
            assert!(matches!(refusal, VersionRefusal::Malformed(_)));
            assert_eq!(refusal.to_rpc_error().code, crate::errors::INVALID_PARAMS);
        }
    }

    #[test]
    fn the_wire_form_is_a_string_and_integers_are_rejected() {
        let version = SemVer::new(1, 2, 3);
        assert_eq!(serde_json::to_value(version).unwrap(), json!("1.2.3"));
        assert_eq!(
            serde_json::from_value::<SemVer>(json!("1.2.3")).unwrap(),
            version
        );
        assert!(serde_json::from_value::<SemVer>(json!(48)).is_err());
        assert!(serde_json::from_value::<SemVer>(json!("1.2")).is_err());
        let schema = schemars::schema_for!(SemVer);
        assert_eq!(schema.get("type"), Some(&json!("string")));
    }

    #[test]
    fn the_peer_version_is_read_before_the_rest_of_the_handshake() {
        assert_eq!(
            peer_protocol_version(&json!({"protocol_version": "2.1.0", "client": 7})),
            Ok(SemVer::new(2, 1, 0))
        );
        assert_eq!(
            peer_protocol_version(&json!({"protocol_version": 48})),
            Err(PeerVersionError::Legacy(48))
        );
        assert_eq!(
            peer_protocol_version(&json!({"client": {}})),
            Err(PeerVersionError::Missing)
        );
        for malformed in [json!("1.2"), json!(-1), json!(1.5), json!(null), json!([1])] {
            assert_eq!(
                peer_protocol_version(&json!({"protocol_version": malformed})),
                Err(PeerVersionError::Malformed)
            );
        }
        assert_eq!(
            peer_protocol_version(&json!(null)),
            Err(PeerVersionError::Missing)
        );
    }
}
