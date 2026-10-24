//! User content and media references, as specified in
//! `docs/site/specs/compatibility-v1.md` section 11.
//!
//! The params of `run.start`, `run.steer`, and `session.submit_input`, the
//! input-lane events, `message_injected`, and producer message bodies carry
//! [`UserContent`].

use std::{borrow::Cow, fmt};

use schemars::{JsonSchema, Schema, SchemaGenerator, json_schema};
use serde::{Deserialize, Serialize};

use crate::Open;

/// A content-addressed artifact in a session tree's store: a user upload, a
/// tool attachment, or the full content of a truncated display block.
#[derive(Clone, Debug, Deserialize, Eq, Hash, JsonSchema, PartialEq, Serialize)]
pub struct ArtifactRef {
    /// Lowercase hex SHA-256 of the bytes.
    #[schemars(length(min = 64, max = 64), regex(pattern = "^[0-9a-f]{64}$"))]
    pub sha256: String,
    pub byte_length: u64,
    /// The media type sniffed from the content, such as `image/png`.
    #[schemars(length(min = 3, max = 255))]
    pub media_type: String,
    /// The name the file was attached under, without a directory.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(length(min = 1, max = 255))]
    pub filename: Option<String>,
}

impl ArtifactRef {
    pub const MAX_MEDIA_TYPE_BYTES: usize = 255;
    pub const MAX_FILENAME_BYTES: usize = 255;

    /// Checks the reference as a writer must before sending it.
    pub fn validate(&self) -> Result<(), ContentError> {
        if self.sha256.len() != 64
            || !self
                .sha256
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        {
            return Err(ContentError::Digest);
        }
        let media_type_valid = self.media_type.len() <= Self::MAX_MEDIA_TYPE_BYTES
            && self
                .media_type
                .split_once('/')
                .is_some_and(|(kind, subtype)| media_token(kind) && media_token(subtype));
        if !media_type_valid {
            return Err(ContentError::MediaType);
        }
        if let Some(filename) = &self.filename
            && (filename.is_empty()
                || filename.len() > Self::MAX_FILENAME_BYTES
                || filename
                    .chars()
                    .any(|character| character.is_control() || matches!(character, '/' | '\\')))
        {
            return Err(ContentError::Filename);
        }
        Ok(())
    }

    /// The media kind the media type names: `image` for `image/png`.
    #[must_use]
    pub fn top_level_type(&self) -> &str {
        self.media_type
            .split_once('/')
            .map_or(self.media_type.as_str(), |(kind, _)| kind)
    }

    /// The filename, or a short form of the digest for an unnamed artifact.
    #[must_use]
    pub fn display_name(&self) -> &str {
        self.filename
            .as_deref()
            .unwrap_or_else(|| self.sha256.get(..12).unwrap_or(&self.sha256))
    }
}

fn media_token(token: &str) -> bool {
    !token.is_empty()
        && token.bytes().enumerate().all(|(index, byte)| {
            byte.is_ascii_lowercase()
                || byte.is_ascii_digit()
                || (index > 0 && b"!#$&^_.+-".contains(&byte))
        })
}

/// A byte count as people read it: `512 B`, `471 KB`, `3.8 MB`.
#[must_use]
pub fn human_bytes(bytes: u64) -> String {
    const KIB: u64 = 1024;
    const MIB: u64 = KIB * 1024;
    const GIB: u64 = MIB * 1024;
    #[allow(clippy::cast_precision_loss)]
    let scaled = |unit: u64| bytes as f64 / unit as f64;
    match bytes {
        bytes if bytes < KIB => format!("{bytes} B"),
        bytes if bytes < MIB => format!("{:.0} KB", scaled(KIB)),
        bytes if bytes < GIB => format!("{:.1} MB", scaled(MIB)),
        _ => format!("{:.1} GB", scaled(GIB)),
    }
}

/// One part of a user's input.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum UserContentPart {
    Text { text: String },
    File { artifact: ArtifactRef },
}

crate::open_enum!(tagged UserContentPart, tag = "type", tags = [
    "file",
    "text",
]);

/// A user's input: a nonempty list of parts. An unknown part, from a newer
/// client, is kept as is and is never sent to a model.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(transparent)]
pub struct UserContent(Vec<Open<UserContentPart>>);

impl UserContent {
    /// Content of `parts`, which must not be empty.
    pub fn new(parts: Vec<Open<UserContentPart>>) -> Result<Self, ContentError> {
        if parts.is_empty() {
            return Err(ContentError::Empty);
        }
        Ok(Self(parts))
    }

    /// Content of one text part.
    #[must_use]
    pub fn text(text: impl Into<String>) -> Self {
        Self(vec![Open::Known(UserContentPart::Text {
            text: text.into(),
        })])
    }

    #[must_use]
    pub fn parts(&self) -> &[Open<UserContentPart>] {
        &self.0
    }

    #[must_use]
    pub fn into_parts(self) -> Vec<Open<UserContentPart>> {
        self.0
    }

    /// Checks the content as a writer must before storing it: every file
    /// part names a valid artifact reference.
    pub fn validate(&self) -> Result<(), ContentError> {
        self.files().try_for_each(ArtifactRef::validate)
    }

    /// Whether every part is a text part. `message_injected` and producer
    /// message bodies accept nothing else yet.
    #[must_use]
    pub fn is_text_only(&self) -> bool {
        self.0
            .iter()
            .all(|part| matches!(part, Open::Known(UserContentPart::Text { .. })))
    }

    /// [`Self::validate`] for content that may carry text parts only.
    pub fn validate_text_only(&self) -> Result<(), ContentError> {
        if self.is_text_only() {
            Ok(())
        } else {
            Err(ContentError::NotText)
        }
    }

    /// Whether the content has no text but whitespace and no other part.
    #[must_use]
    pub fn is_blank(&self) -> bool {
        self.0.iter().all(|part| {
            matches!(part, Open::Known(UserContentPart::Text { text }) if text.trim().is_empty())
        })
    }

    /// The text of content that is exactly one text part.
    #[must_use]
    pub fn single_text(&self) -> Option<&str> {
        match self.0.as_slice() {
            [Open::Known(UserContentPart::Text { text })] => Some(text),
            _ => None,
        }
    }

    /// The text parts' texts, in order.
    pub fn texts(&self) -> impl Iterator<Item = &str> {
        self.0.iter().filter_map(|part| match part {
            Open::Known(UserContentPart::Text { text }) => Some(text.as_str()),
            _ => None,
        })
    }

    /// The artifacts of the file parts, in order.
    pub fn files(&self) -> impl Iterator<Item = &ArtifactRef> {
        self.0.iter().filter_map(|part| match part {
            Open::Known(UserContentPart::File { artifact }) => Some(artifact),
            _ => None,
        })
    }

    /// The content as one string, for code that still needs one: text parts
    /// as they are, file parts as a short placeholder, joined by newlines.
    /// Content of one text part is exactly that text.
    #[must_use]
    pub fn as_plain_text(&self) -> Cow<'_, str> {
        if let [Open::Known(UserContentPart::Text { text })] = self.0.as_slice() {
            return Cow::Borrowed(text);
        }
        let parts = self
            .0
            .iter()
            .map(|part| match part {
                Open::Known(UserContentPart::Text { text }) => Cow::Borrowed(text.as_str()),
                Open::Known(UserContentPart::File { artifact }) => Cow::Owned(format!(
                    "[attachment {} ({}, {})]",
                    artifact.display_name(),
                    artifact.media_type,
                    human_bytes(artifact.byte_length)
                )),
                Open::Unknown { tag, .. } => Cow::Owned(format!("[unsupported content: {tag}]")),
            })
            .collect::<Vec<_>>();
        Cow::Owned(parts.join("\n"))
    }
}

/// Content of one text part.
impl From<String> for UserContent {
    fn from(text: String) -> Self {
        Self::text(text)
    }
}

/// Content of one text part.
impl From<&str> for UserContent {
    fn from(text: &str) -> Self {
        Self::text(text)
    }
}

/// Content equals a text when it is exactly one text part of that text.
impl PartialEq<str> for UserContent {
    fn eq(&self, other: &str) -> bool {
        matches!(self.0.as_slice(), [Open::Known(UserContentPart::Text { text })] if text == other)
    }
}

impl PartialEq<&str> for UserContent {
    fn eq(&self, other: &&str) -> bool {
        self == *other
    }
}

impl PartialEq<String> for UserContent {
    fn eq(&self, other: &String) -> bool {
        self == other.as_str()
    }
}

impl<'de> Deserialize<'de> for UserContent {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        Self::new(Vec::deserialize(deserializer)?).map_err(serde::de::Error::custom)
    }
}

impl JsonSchema for UserContent {
    fn schema_name() -> Cow<'static, str> {
        Cow::Borrowed("UserContent")
    }

    fn json_schema(generator: &mut SchemaGenerator) -> Schema {
        let items = generator.subschema_for::<Open<UserContentPart>>();
        json_schema!({
            "type": "array",
            "items": items,
            "minItems": 1,
            "description": "A user's input: a nonempty list of parts."
        })
    }
}

crate::snake_label!(
    /// A media kind label: `image`, `pdf`, `audio`, `video`, or (an ACP
    /// agent's, client protocol 1.26.0) `text` today. A kind this build does
    /// not know is shown by its label and never offered.
    MediaKindLabel,
    "media kind",
    "A media kind, such as `image`."
);

impl MediaKindLabel {
    pub const IMAGE: &'static str = "image";
    pub const PDF: &'static str = "pdf";
    pub const AUDIO: &'static str = "audio";
    pub const VIDEO: &'static str = "video";
    /// A text file, which an ACP agent takes in its prompt (client protocol
    /// 1.26.0).
    pub const TEXT: &'static str = "text";
}

/// What one model accepts of one media kind in user input. The limits are
/// effective: the lowest of the model's capability, its provider's delivery
/// limits, and the attachment caps.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
pub struct InputMedia {
    pub kind: MediaKindLabel,
    /// The media types accepted, such as `image/png`.
    pub mime_types: Vec<String>,
    /// The largest accepted file.
    pub max_bytes: u64,
    /// The longest accepted side of an image or video, in pixels.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_dimension: Option<u32>,
    /// The most pixels an accepted image or video frame has.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_pixels: Option<u64>,
    /// The most parts of this kind one input carries.
    pub max_count: u32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
pub enum ContentError {
    #[error("content must have at least one part")]
    Empty,
    #[error("an artifact digest is 64 lowercase hex digits")]
    Digest,
    #[error("an artifact media type is a lowercase type/subtype of at most 255 bytes")]
    MediaType,
    #[error("an artifact filename is 1 to 255 bytes without control characters or slashes")]
    Filename,
    #[error("only text parts are accepted here")]
    NotText,
}

impl fmt::Display for UserContent {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.as_plain_text())
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn png() -> ArtifactRef {
        ArtifactRef {
            sha256: "ab".repeat(32),
            byte_length: 482_113,
            media_type: "image/png".into(),
            filename: Some("screenshot.png".into()),
        }
    }

    #[test]
    fn artifact_references_are_validated_for_writers() {
        assert_eq!(png().validate(), Ok(()));
        assert_eq!(png().top_level_type(), "image");
        for (change, expected) in [
            (
                (|artifact: &mut ArtifactRef| artifact.sha256 = "AB".repeat(32))
                    as fn(&mut ArtifactRef),
                ContentError::Digest,
            ),
            (
                |artifact| artifact.sha256 = "ab".repeat(31),
                ContentError::Digest,
            ),
            (
                |artifact| artifact.media_type = "image".into(),
                ContentError::MediaType,
            ),
            (
                |artifact| artifact.media_type = "Image/PNG".into(),
                ContentError::MediaType,
            ),
            (
                |artifact| artifact.media_type = "image/".into(),
                ContentError::MediaType,
            ),
            (
                |artifact| artifact.media_type = format!("image/{}", "a".repeat(250)),
                ContentError::MediaType,
            ),
            (
                |artifact| artifact.filename = Some(String::new()),
                ContentError::Filename,
            ),
            (
                |artifact| artifact.filename = Some("../x.png".into()),
                ContentError::Filename,
            ),
            (
                |artifact| artifact.filename = Some("a\nb".into()),
                ContentError::Filename,
            ),
            (
                |artifact| artifact.filename = Some("a".repeat(256)),
                ContentError::Filename,
            ),
        ] {
            let mut artifact = png();
            change(&mut artifact);
            assert_eq!(artifact.validate(), Err(expected), "{artifact:?}");
        }
        let mut unnamed = png();
        unnamed.filename = None;
        assert_eq!(unnamed.validate(), Ok(()));
        assert_eq!(unnamed.display_name(), "abababababab");
        unnamed.media_type = "application/vnd.api+json".into();
        assert_eq!(unnamed.validate(), Ok(()));
    }

    #[test]
    fn byte_counts_read_like_the_spec_examples() {
        assert_eq!(human_bytes(0), "0 B");
        assert_eq!(human_bytes(1023), "1023 B");
        assert_eq!(human_bytes(1024), "1 KB");
        assert_eq!(human_bytes(482_113), "471 KB");
        assert_eq!(human_bytes(3_932_160), "3.8 MB");
        assert_eq!(human_bytes(5 * 1024 * 1024 * 1024), "5.0 GB");
    }

    #[test]
    fn user_content_is_a_nonempty_open_list() {
        let wire = json!([
            {"type": "text", "text": "What is wrong with this layout?"},
            {"type": "file", "artifact": png()},
            {"type": "sticker", "id": 7}
        ]);
        let content: UserContent = serde_json::from_value(wire.clone()).unwrap();
        assert_eq!(content.parts().len(), 3);
        assert!(content.parts()[2].is_unknown());
        assert_eq!(content.files().collect::<Vec<_>>(), [&png()]);
        assert_eq!(serde_json::to_value(&content).unwrap(), wire);
        assert!(serde_json::from_value::<UserContent>(json!([])).is_err());
        assert!(serde_json::from_value::<UserContent>(json!("text")).is_err());
        assert_eq!(UserContent::new(Vec::new()), Err(ContentError::Empty));

        let schema = schemars::schema_for!(UserContent);
        assert_eq!(schema.get("minItems"), Some(&json!(1)));
        assert!(schema.as_value().to_string().contains("\"x-open\":true"));
    }

    #[test]
    fn plain_text_keeps_a_single_text_part_exact_and_marks_files() {
        let single = UserContent::text("  line one\nline two ");
        assert_eq!(single.as_plain_text(), "  line one\nline two ");
        assert_eq!(single, "  line one\nline two ");
        assert_ne!(UserContent::text("a\nb"), "a");
        assert_eq!(
            serde_json::to_value(&single).unwrap(),
            json!([{"type": "text", "text": "  line one\nline two "}])
        );
        let mixed: UserContent = serde_json::from_value(json!([
            {"type": "text", "text": "look"},
            {"type": "file", "artifact": png()},
            {"type": "sticker"}
        ]))
        .unwrap();
        assert_eq!(
            mixed.as_plain_text(),
            "look\n[attachment screenshot.png (image/png, 471 KB)]\n[unsupported content: sticker]"
        );
        assert_eq!(mixed.to_string(), mixed.as_plain_text());
    }

    #[test]
    fn input_media_states_the_effective_limits() {
        let image: InputMedia = serde_json::from_value(json!({
            "kind": "image", "mime_types": ["image/png", "image/jpeg"],
            "max_bytes": 3_932_160, "max_dimension": 8000, "max_pixels": 16_777_216,
            "max_count": 20
        }))
        .unwrap();
        assert_eq!(image.kind.as_str(), MediaKindLabel::IMAGE);
        assert_eq!(image.max_dimension, Some(8000));
        let video: InputMedia = serde_json::from_value(json!({
            "kind": "video", "mime_types": ["video/mp4"], "max_bytes": 19_608_371, "max_count": 1
        }))
        .unwrap();
        assert_eq!(video.max_pixels, None);
        assert_eq!(
            serde_json::to_value(&video).unwrap(),
            json!({"kind": "video", "mime_types": ["video/mp4"], "max_bytes": 19_608_371, "max_count": 1})
        );
        assert!(
            serde_json::from_value::<InputMedia>(json!({
                "kind": "Image", "mime_types": [], "max_bytes": 1, "max_count": 1
            }))
            .is_err()
        );
    }
}
