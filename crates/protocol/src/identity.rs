use std::{borrow::Cow, fmt, str::FromStr};

use schemars::{JsonSchema, Schema, SchemaGenerator, json_schema};
use serde::{Deserialize, Serialize};
use ts_rs::TS;
use uuid::Uuid;

/// Exact pinned models.dev catalog revision used by protocol v7.
pub const PINNED_CATALOG_REVISION: &str =
    "sha256:d65af0b058204954f6b08af537fa13e91f251c618d69d8c20a2d5915731d482a";

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum WireStringError {
    Empty,
    TooLong { found: usize, maximum: usize },
    Invalid(&'static str),
}

impl fmt::Display for WireStringError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Empty => formatter.write_str("value must not be empty"),
            Self::TooLong { found, maximum } => write!(
                formatter,
                "value is {found} bytes, exceeding the {maximum}-byte limit"
            ),
            Self::Invalid(requirement) => write!(formatter, "value {requirement}"),
        }
    }
}

impl std::error::Error for WireStringError {}

fn validate_bounded(value: &str, maximum: usize) -> Result<(), WireStringError> {
    if value.is_empty() {
        return Err(WireStringError::Empty);
    }
    if value.len() > maximum {
        return Err(WireStringError::TooLong {
            found: value.len(),
            maximum,
        });
    }
    Ok(())
}

fn validate_control_free(value: &str, maximum: usize) -> Result<(), WireStringError> {
    validate_bounded(value, maximum)?;
    if value.chars().any(char::is_control) {
        return Err(WireStringError::Invalid(
            "must not contain control characters",
        ));
    }
    Ok(())
}

macro_rules! string_wire_impl {
    ($name:ident, $max:expr, $pattern:literal, $description:literal, $validator:expr) => {
        impl $name {
            pub const MAX_BYTES: usize = $max;

            pub fn new(value: impl Into<String>) -> Result<Self, WireStringError> {
                let value = value.into();
                ($validator)(&value)?;
                Ok(Self(value))
            }

            #[must_use]
            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                self.0.fmt(formatter)
            }
        }

        impl Serialize for $name {
            fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
            where
                S: serde::Serializer,
            {
                serializer.serialize_str(&self.0)
            }
        }

        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
            where
                D: serde::Deserializer<'de>,
            {
                Self::new(String::deserialize(deserializer)?).map_err(serde::de::Error::custom)
            }
        }

        impl JsonSchema for $name {
            fn inline_schema() -> bool {
                true
            }

            fn schema_name() -> Cow<'static, str> {
                Cow::Borrowed(stringify!($name))
            }

            fn json_schema(_generator: &mut SchemaGenerator) -> Schema {
                json_schema!({
                    "type": "string",
                    "minLength": 1,
                    "maxLength": $max,
                    "pattern": $pattern,
                    "description": $description
                })
            }
        }
    };
}

macro_rules! bounded_control_free_type {
    ($name:ident, $max:expr, $description:literal) => {
        #[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, TS)]
        #[ts(type = "string")]
        pub struct $name(String);
        string_wire_impl!(
            $name,
            $max,
            "^[^\\p{Cc}\\p{Cf}]+$",
            $description,
            |value: &str| validate_control_free(value, $name::MAX_BYTES)
        );
    };
}

bounded_control_free_type!(ClientRunId, 256, "Bounded run idempotency key.");
bounded_control_free_type!(
    ClientResponseId,
    256,
    "Bounded approval response idempotency key."
);
bounded_control_free_type!(
    ClientConnectId,
    256,
    "Bounded provider connection idempotency key."
);
bounded_control_free_type!(
    ClientRenameId,
    256,
    "Bounded title mutation idempotency key."
);
bounded_control_free_type!(
    ModelCallId,
    512,
    "Bounded semantic model tool-call identity."
);
bounded_control_free_type!(ProviderItemId, 512, "Bounded provider item identity.");
bounded_control_free_type!(CwdIdentity, 4096, "Opaque canonical workspace identity.");
bounded_control_free_type!(
    SafeDisplayText,
    1024,
    "Control-free bounded presentation text."
);
bounded_control_free_type!(
    SafeErrorMessage,
    4096,
    "Control-free bounded safe error text."
);
bounded_control_free_type!(CatalogIdentifier, 1024, "Bounded catalog identity.");

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, TS)]
#[ts(type = "string")]
pub struct CatalogText(String);
string_wire_impl!(
    CatalogText,
    16_384,
    "^\\S(?:.*\\S)?$",
    "Trimmed bounded catalog display text.",
    |value: &str| {
        validate_control_free(value, CatalogText::MAX_BYTES)?;
        if value.trim() != value {
            return Err(WireStringError::Invalid("must be trimmed and nonblank"));
        }
        Ok(())
    }
);

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, TS)]
#[ts(type = "string")]
pub struct CredentialFieldName(String);

string_wire_impl!(
    CredentialFieldName,
    1024,
    "^[A-Z0-9_]+$",
    "Strict provider credential field name.",
    |value: &str| {
        validate_bounded(value, CredentialFieldName::MAX_BYTES)?;
        if !value
            .bytes()
            .all(|byte| byte.is_ascii_uppercase() || byte.is_ascii_digit() || byte == b'_')
        {
            return Err(WireStringError::Invalid(
                "must use only uppercase ASCII letters, digits, or '_'",
            ));
        }
        Ok(())
    }
);

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, TS)]
#[ts(type = "string")]
pub struct CatalogRevision(String);

impl CatalogRevision {
    pub fn current() -> Self {
        Self(PINNED_CATALOG_REVISION.to_owned())
    }

    pub fn new(value: impl Into<String>) -> Result<Self, WireStringError> {
        let value = value.into();
        if value == PINNED_CATALOG_REVISION {
            Ok(Self(value))
        } else {
            Err(WireStringError::Invalid(
                "must equal the pinned models.dev catalog revision",
            ))
        }
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for CatalogRevision {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}
impl Serialize for CatalogRevision {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_str(&self.0)
    }
}
impl<'de> Deserialize<'de> for CatalogRevision {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        Self::new(String::deserialize(deserializer)?).map_err(serde::de::Error::custom)
    }
}
impl JsonSchema for CatalogRevision {
    fn inline_schema() -> bool {
        true
    }
    fn schema_name() -> Cow<'static, str> {
        Cow::Borrowed("CatalogRevision")
    }
    fn json_schema(_generator: &mut SchemaGenerator) -> Schema {
        json_schema!({"type":"string","const":PINNED_CATALOG_REVISION})
    }
}

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, TS)]
#[ts(type = "string")]
pub struct SnapshotRevision(String);

string_wire_impl!(
    SnapshotRevision,
    71,
    "^sha256:[0-9a-f]{64}$",
    "SHA-256 snapshot revision.",
    |value: &str| {
        if value.len() == 71
            && value.starts_with("sha256:")
            && value[7..]
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        {
            Ok(())
        } else {
            Err(WireStringError::Invalid(
                "must be sha256: followed by 64 lowercase hexadecimal characters",
            ))
        }
    }
);

#[derive(Clone, Deserialize, Serialize)]
#[serde(transparent)]
struct TransparentUuid(Uuid);

macro_rules! uuid_id {
    ($name:ident) => {
        #[derive(Clone, Copy, Debug, Eq, Hash, JsonSchema, Ord, PartialEq, PartialOrd, TS)]
        #[schemars(transparent)]
        pub struct $name(pub Uuid);

        impl Serialize for $name {
            fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
            where
                S: serde::Serializer,
            {
                TransparentUuid(self.0).serialize(serializer)
            }
        }

        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
            where
                D: serde::Deserializer<'de>,
            {
                TransparentUuid::deserialize(deserializer).map(|uuid| Self(uuid.0))
            }
        }

        impl $name {
            #[must_use]
            pub fn new_v7() -> Self {
                Self(Uuid::now_v7())
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                self.0.fmt(formatter)
            }
        }

        impl FromStr for $name {
            type Err = uuid::Error;
            fn from_str(value: &str) -> Result<Self, Self::Err> {
                value.parse().map(Self)
            }
        }
    };
}

uuid_id!(SessionId);
uuid_id!(RunId);
uuid_id!(AttemptId);
uuid_id!(ToolCallId);
uuid_id!(InvocationId);
uuid_id!(InternalAgentInvocationId);
uuid_id!(InternalAgentRunId);
uuid_id!(ApprovalId);
uuid_id!(TreeApprovalGrantId);

#[derive(Clone, Eq, Hash, PartialEq, TS)]
#[ts(type = "string")]
pub struct Sha256Digest(String);

impl Sha256Digest {
    pub const HEX_LENGTH: usize = 64;

    pub fn new(value: impl Into<String>) -> Result<Self, WireStringError> {
        let value = value.into();
        if value.len() != Self::HEX_LENGTH
            || !value
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        {
            return Err(WireStringError::Invalid(
                "must be exactly 64 lowercase hexadecimal characters",
            ));
        }
        Ok(Self(value))
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    #[must_use]
    pub fn of_bytes(bytes: &[u8]) -> Self {
        Self(sha256_hex(bytes))
    }

    #[must_use]
    pub(crate) fn hash_domain_separated(domain: &[u8], bytes: &[u8]) -> Self {
        let mut input = Vec::with_capacity(domain.len() + 8 + bytes.len());
        input.extend_from_slice(domain);
        input.extend_from_slice(&(bytes.len() as u64).to_be_bytes());
        input.extend_from_slice(bytes);
        Self(sha256_hex(&input))
    }
}

impl fmt::Debug for Sha256Digest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}
impl fmt::Display for Sha256Digest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}
impl Serialize for Sha256Digest {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_str(&self.0)
    }
}
impl<'de> Deserialize<'de> for Sha256Digest {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        Self::new(String::deserialize(deserializer)?).map_err(serde::de::Error::custom)
    }
}
impl JsonSchema for Sha256Digest {
    fn inline_schema() -> bool {
        true
    }
    fn schema_name() -> Cow<'static, str> {
        Cow::Borrowed("Sha256Digest")
    }
    fn json_schema(_generator: &mut SchemaGenerator) -> Schema {
        json_schema!({"type":"string","pattern":"^[0-9a-f]{64}$","minLength":64,"maxLength":64})
    }
}

fn sha256_hex(input: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(input);
    let mut output = String::with_capacity(64);
    for byte in digest {
        use std::fmt::Write as _;
        write!(&mut output, "{byte:02x}").expect("writing to a string cannot fail");
    }
    output
}
