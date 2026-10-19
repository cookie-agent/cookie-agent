//! Uploading user media into a session tree's artifact store, and reading
//! any artifact of a tree back, as specified in
//! `docs/site/specs/compatibility-v1.md` section 11.3.
//!
//! An upload is begun, sent in chunks of at most [`ARTIFACT_CHUNK_BYTES`]
//! in order, and finished with the digest of its bytes, which returns the
//! [`ArtifactRef`] a `file` content part names. Bytes travel as base64.

use std::{borrow::Cow, fmt};

use schemars::{JsonSchema, Schema, SchemaGenerator, json_schema};
use serde::{Deserialize, Serialize};

use crate::{ArtifactRef, SessionId, WireStringError};

pub const ARTIFACT_UPLOAD_BEGIN_METHOD: &str = "artifact.upload.begin";
pub const ARTIFACT_UPLOAD_CHUNK_METHOD: &str = "artifact.upload.chunk";
pub const ARTIFACT_UPLOAD_FINISH_METHOD: &str = "artifact.upload.finish";
pub const ARTIFACT_UPLOAD_CANCEL_METHOD: &str = "artifact.upload.cancel";
pub const ARTIFACT_READ_METHOD: &str = "artifact.read";

/// The most decoded bytes one `artifact.upload.chunk` carries or one
/// `artifact.read` returns.
pub const ARTIFACT_CHUNK_BYTES: u64 = 1024 * 1024;

/// The longest base64 text of [`ARTIFACT_CHUNK_BYTES`] bytes.
const MAX_CHUNK_BASE64_LEN: u64 = ARTIFACT_CHUNK_BYTES.div_ceil(3) * 4;

/// An upload in progress, unique within the daemon and valid only on the
/// connection that began it.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(transparent)]
pub struct UploadId(String);

impl UploadId {
    pub const MAX_LEN: usize = 64;

    pub fn new(value: impl Into<String>) -> Result<Self, WireStringError> {
        let value = value.into();
        let valid = !value.is_empty()
            && value.len() <= Self::MAX_LEN
            && value
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_');
        if valid {
            Ok(Self(value))
        } else {
            Err(WireStringError::Invalid(
                "an upload ID is 1 to 64 ASCII letters, digits, `-`, or `_`",
            ))
        }
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for UploadId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for UploadId {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        Self::new(String::deserialize(deserializer)?).map_err(serde::de::Error::custom)
    }
}

impl JsonSchema for UploadId {
    fn inline_schema() -> bool {
        true
    }
    fn schema_name() -> Cow<'static, str> {
        Cow::Borrowed("UploadId")
    }
    fn json_schema(_: &mut SchemaGenerator) -> Schema {
        json_schema!({
            "type": "string",
            "pattern": "^[A-Za-z0-9_-]{1,64}$",
            "description": "An upload in progress on this connection."
        })
    }
}

/// `artifact.upload.begin`: starts an upload of `byte_length` bytes into
/// the store of `session_id`'s tree.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ArtifactUploadBeginParams {
    pub session_id: SessionId,
    /// The upload's size; at most the largest attachment cap (25 MiB).
    #[schemars(range(min = 1))]
    pub byte_length: u64,
    /// The name to attach the file under, without a directory. It never
    /// decides the media type.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(length(min = 1, max = 255))]
    pub filename: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
pub struct ArtifactUploadBeginResult {
    pub upload_id: UploadId,
    /// The most decoded bytes one chunk may carry.
    pub chunk_bytes: u64,
}

/// `artifact.upload.chunk`: the bytes at `offset`, which must be exactly
/// the bytes received so far. Send a chunk once the previous one is
/// answered: requests run concurrently, so pipelined chunks may arrive out
/// of order and be refused.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ArtifactUploadChunkParams {
    pub upload_id: UploadId,
    pub offset: u64,
    /// Base64 (standard alphabet, padded) of at most `chunk_bytes` bytes.
    #[schemars(length(min = 1, max = MAX_CHUNK_BASE64_LEN))]
    pub data: String,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
pub struct ArtifactUploadChunkResult {}

/// `artifact.upload.finish`: checks the digest, recognizes and validates
/// the content, and stores it.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ArtifactUploadFinishParams {
    pub upload_id: UploadId,
    /// Lowercase hex SHA-256 of every byte sent.
    #[schemars(length(min = 64, max = 64), regex(pattern = "^[0-9a-f]{64}$"))]
    pub sha256: String,
}

/// The stored upload: the reference a `file` part names, and an image's
/// size.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
pub struct ArtifactUploadFinishResult {
    pub artifact: ArtifactRef,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image: Option<ImageSize>,
}

/// An image's width and height, in pixels.
#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
pub struct ImageSize {
    pub width: u32,
    pub height: u32,
}

/// `artifact.upload.cancel`: drops an upload in progress.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ArtifactUploadCancelParams {
    pub upload_id: UploadId,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
pub struct ArtifactUploadCancelResult {}

/// `artifact.read`: `length` bytes from `offset` of an artifact in the
/// store of `session_id`'s tree: a user upload, a tool attachment, or the
/// `full` content of a display block.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ArtifactReadParams {
    pub session_id: SessionId,
    #[schemars(length(min = 64, max = 64), regex(pattern = "^[0-9a-f]{64}$"))]
    pub sha256: String,
    /// At most `total_bytes`; reading at `total_bytes` returns no data.
    pub offset: u64,
    #[schemars(range(min = 1, max = ARTIFACT_CHUNK_BYTES))]
    pub length: u64,
}

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
pub struct ArtifactReadResult {
    /// Base64 of the bytes read: fewer than `length` at the end.
    pub data: String,
    pub total_bytes: u64,
    /// The media type recognized from the content; `text/plain` for UTF-8
    /// text and `application/octet-stream` for anything else.
    pub media_type: String,
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn upload_ids_are_short_url_safe_labels() {
        assert!(UploadId::new("upl_0192-abc").is_ok());
        for invalid in ["", "has space", "slash/", &"a".repeat(65)] {
            assert!(UploadId::new(invalid).is_err(), "{invalid:?}");
        }
        assert!(serde_json::from_value::<UploadId>(json!("a b")).is_err());
    }

    #[test]
    fn a_chunk_of_the_largest_size_fits_its_base64_bound() {
        // Padded base64 is four characters per three bytes, rounded up.
        assert_eq!(MAX_CHUNK_BASE64_LEN, 1_398_104);
    }

    #[test]
    fn finish_results_nest_the_reference_a_file_part_names() {
        let result = ArtifactUploadFinishResult {
            artifact: ArtifactRef {
                sha256: "ab".repeat(32),
                byte_length: 3,
                media_type: "image/png".into(),
                filename: Some("a.png".into()),
            },
            image: Some(ImageSize {
                width: 2,
                height: 1,
            }),
        };
        assert_eq!(
            serde_json::to_value(&result).unwrap(),
            json!({
                "artifact": {"sha256": "ab".repeat(32), "byte_length": 3,
                             "media_type": "image/png", "filename": "a.png"},
                "image": {"width": 2, "height": 1}
            })
        );
    }
}
