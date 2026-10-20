//! Tool attachments: recognizing and validating media files, and deciding
//! how a turn's model receives them.

use std::path::Path;

use crate::ToolError;
pub use cookie_agent_protocol::media_sniff::{
    MAX_ATTACHMENT_BYTES, MAX_IMAGE_DIMENSION, MAX_IMAGE_PIXELS, MAX_VIDEO_ATTACHMENT_BYTES,
    UploadRejection, ValidatedUpload, canonical_video_mime_type, max_attachment_bytes, media_kind,
    mime_type_matches, recognize_media_type, validate_upload,
};
use cookie_agent_protocol::media_sniff::{
    MediaKind, known_media_extension, media_extension_matches, recognize, validate_image,
    validate_pdf,
};
use cookie_agent_protocol::{
    AdaptorId, InputMedia, MediaCapability, MediaKind as CapabilityMediaKind, MediaKindLabel,
    ModelCapabilities, ModelKey,
    runtime_protocol::{MediaDelivery, ToolInputMedia},
};

const MAX_ENCODED_BYTES: usize = 20 * 1024 * 1024;
const MAX_VIDEO_ENCODED_BYTES: usize = 25 * 1024 * 1024;
/// Anthropic caps each image at 10 MiB of base64; the largest raw size that encodes within it.
const ANTHROPIC_IMAGE_BYTES: u64 = 10 * 1024 * 1024 / 4 * 3;
/// Vertex caps each inline image at 7 MiB.
const VERTEX_IMAGE_BYTES: u64 = 7 * 1024 * 1024;
/// The Gemini API caps the whole request at 20 MiB, inline base64 included; this raw size
/// encodes to about 18.7 MiB, leaving headroom for the rest of the request.
const GEMINI_INLINE_BYTES: u64 = 14 * 1024 * 1024;
const BEDROCK_IMAGE_BYTES: u64 = 15 * 1024 * 1024 / 4;
const BEDROCK_PDF_BYTES: u64 = 9 * 1024 * 1024 / 2;
/// Bedrock requires inline video base64 strictly below 25 MiB; the largest raw size whose
/// base64 encoding stays below that limit (4 * ceil(n / 3) < 26,214,400).
const BEDROCK_VIDEO_BYTES: u64 = 19_660_797;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AttachmentGate {
    AttachToolResult,
    DeliverViaUserTurn,
    RejectUnsupportedModel,
    RejectUnsupportedFamily,
    RejectTooLarge { max_bytes: u64 },
}

#[must_use]
pub fn attachment_gate_error(
    gate: AttachmentGate,
    mime_type: &str,
    model: &ModelKey,
    family: &AdaptorId,
) -> Option<String> {
    match gate {
        AttachmentGate::AttachToolResult | AttachmentGate::DeliverViaUserTurn => None,
        AttachmentGate::RejectUnsupportedModel => {
            let input = if mime_type.starts_with("image/") {
                "image"
            } else if mime_type == "application/pdf" {
                "PDF"
            } else if mime_type.starts_with("audio/") {
                "audio"
            } else if mime_type.starts_with("video/") {
                "video"
            } else {
                "media"
            };
            Some(format!(
                "Cannot attach {mime_type}: the active model \"{model}\" does not accept {input} inputs"
            ))
        }
        AttachmentGate::RejectUnsupportedFamily => Some(format!(
            "Cannot attach {mime_type}: not deliverable in tool results or user messages via the {} family API",
            family.as_str()
        )),
        AttachmentGate::RejectTooLarge { max_bytes } => Some(format!(
            "Cannot attach {mime_type}: exceeds the {} MiB inline limit for this provider",
            format_mib(max_bytes)
        )),
    }
}

fn format_mib(bytes: u64) -> String {
    let mut value = format!("{:.2}", bytes as f64 / (1024 * 1024) as f64);
    while value.ends_with('0') {
        value.pop();
    }
    if value.ends_with('.') {
        value.pop();
    }
    value
}

/// Exact model adapter family of a turn. Unlike [`AdaptorId`] it keeps
/// compatible families apart, because their media delivery differs. It never
/// leaves the daemon: tools see what it implies as [`ToolInputMedia`].
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum AdapterFamily {
    Anthropic,
    AnthropicCompatible,
    OpenaiChat,
    OpenaiResponses,
    OpenaiCompatible,
    GoogleGemini,
    GoogleVertexGemini,
    AwsBedrockConverse,
    AzureOpenaiChat,
    AzureOpenaiResponses,
    CohereV2Chat,
}

/// The media a model accepts from a tool: every kind of `capabilities` that
/// `family` can deliver, with the lower of the model's and the family's byte
/// limit, and how it is delivered.
#[must_use]
pub fn tool_input_media(
    family: AdapterFamily,
    capabilities: &ModelCapabilities,
) -> Vec<ToolInputMedia> {
    deliverable_media(family, capabilities)
        .map(
            |(kind, capability, delivery, family_limit)| ToolInputMedia {
                kind: MediaKindLabel::new(media_kind_label(kind)).expect("static media kind"),
                mime_types: capability
                    .mime_types
                    .iter()
                    .map(|mime_type| mime_type.as_str().to_owned())
                    .collect(),
                max_bytes: capability.max_bytes.min(family_limit),
                delivery,
            },
        )
        .collect()
}

/// The media a model of `capabilities` behind `family` accepts in a user
/// turn: what clients are told it accepts and what input is admitted by
/// (`docs/site/specs/compatibility-v1.md` section 11.2). User content always
/// travels in a user turn, so every kind `family` can deliver at all counts,
/// unlike in [`tool_input_media`]. The media types are those of the
/// model's that an upload can be recognized as; the byte limit is the lowest
/// of the model's, the family's, and the attachment cap; images also carry
/// the dimension and pixel limits validation applies. A kind left with no
/// media type is left out.
#[must_use]
pub fn user_input_media(
    family: AdapterFamily,
    capabilities: &ModelCapabilities,
) -> Vec<InputMedia> {
    deliverable_media(family, capabilities)
        .filter_map(|(kind, capability, _, family_limit)| {
            let label = media_kind_label(kind);
            let mime_types = UPLOAD_MEDIA_TYPES
                .iter()
                .filter(|(of, mime_type)| {
                    *of == label
                        && capability
                            .mime_types
                            .iter()
                            .any(|accepted| mime_type_matches(accepted.as_str(), mime_type))
                })
                .map(|(_, mime_type)| (*mime_type).to_owned())
                .collect::<Vec<_>>();
            if mime_types.is_empty() {
                return None;
            }
            let image = kind == CapabilityMediaKind::Image;
            Some(InputMedia {
                kind: MediaKindLabel::new(label).expect("static media kind"),
                mime_types,
                max_bytes: capability
                    .max_bytes
                    .min(family_limit)
                    .min(max_attachment_bytes(label)),
                max_dimension: image.then_some(MAX_IMAGE_DIMENSION),
                max_pixels: image.then_some(MAX_IMAGE_PIXELS),
                max_count: capability.max_count,
            })
        })
        .collect()
}

/// Every kind of `capabilities` that `family` can deliver, with how it
/// delivers a tool's attachment of it and its byte limit; the one table
/// [`tool_input_media`] and [`user_input_media`] share.
fn deliverable_media(
    family: AdapterFamily,
    capabilities: &ModelCapabilities,
) -> impl Iterator<Item = (CapabilityMediaKind, &MediaCapability, MediaDelivery, u64)> {
    capabilities
        .media
        .iter()
        .filter_map(move |(kind, capability)| {
            let (delivery, family_limit) = family_delivery(family, *kind)?;
            Some((*kind, capability, delivery, family_limit))
        })
}

/// The media types an upload's content can be recognized as, by kind. The
/// Matroska type needs a file extension, which uploads ignore, so a Matroska
/// upload is recognized as WebM.
const UPLOAD_MEDIA_TYPES: &[(&str, &str)] = &[
    (MediaKindLabel::IMAGE, "image/gif"),
    (MediaKindLabel::IMAGE, "image/jpeg"),
    (MediaKindLabel::IMAGE, "image/png"),
    (MediaKindLabel::IMAGE, "image/webp"),
    (MediaKindLabel::PDF, "application/pdf"),
    (MediaKindLabel::AUDIO, "audio/flac"),
    (MediaKindLabel::AUDIO, "audio/mpeg"),
    (MediaKindLabel::AUDIO, "audio/ogg"),
    (MediaKindLabel::AUDIO, "audio/wav"),
    (MediaKindLabel::VIDEO, "video/3gpp"),
    (MediaKindLabel::VIDEO, "video/mp4"),
    (MediaKindLabel::VIDEO, "video/mpeg"),
    (MediaKindLabel::VIDEO, "video/quicktime"),
    (MediaKindLabel::VIDEO, "video/webm"),
    (MediaKindLabel::VIDEO, "video/wmv"),
    (MediaKindLabel::VIDEO, "video/x-flv"),
    (MediaKindLabel::VIDEO, "video/x-msvideo"),
];

const fn media_kind_label(kind: CapabilityMediaKind) -> &'static str {
    match kind {
        CapabilityMediaKind::Image => MediaKindLabel::IMAGE,
        CapabilityMediaKind::Pdf => MediaKindLabel::PDF,
        CapabilityMediaKind::Audio => MediaKindLabel::AUDIO,
        CapabilityMediaKind::Video => MediaKindLabel::VIDEO,
    }
}

/// How `family` delivers a tool's attachment of `kind`, and its byte limit;
/// `None` when it cannot.
fn family_delivery(
    family: AdapterFamily,
    kind: CapabilityMediaKind,
) -> Option<(MediaDelivery, u64)> {
    Some(match (family, kind) {
        (
            AdapterFamily::Anthropic | AdapterFamily::AnthropicCompatible,
            CapabilityMediaKind::Image,
        ) => (MediaDelivery::ToolResult, ANTHROPIC_IMAGE_BYTES),
        (
            AdapterFamily::Anthropic | AdapterFamily::AnthropicCompatible,
            CapabilityMediaKind::Pdf,
        ) => (MediaDelivery::ToolResult, MAX_ENCODED_BYTES as u64),
        (AdapterFamily::AwsBedrockConverse, CapabilityMediaKind::Image) => {
            (MediaDelivery::ToolResult, BEDROCK_IMAGE_BYTES)
        }
        (AdapterFamily::AwsBedrockConverse, CapabilityMediaKind::Pdf) => {
            (MediaDelivery::ToolResult, BEDROCK_PDF_BYTES)
        }
        (AdapterFamily::AwsBedrockConverse, CapabilityMediaKind::Video) => {
            (MediaDelivery::ToolResult, BEDROCK_VIDEO_BYTES)
        }
        (
            AdapterFamily::OpenaiResponses | AdapterFamily::AzureOpenaiResponses,
            CapabilityMediaKind::Image | CapabilityMediaKind::Pdf,
        ) => (MediaDelivery::ToolResult, MAX_ENCODED_BYTES as u64),
        // Families that cannot carry the kind in tool results fall back to a follow-up
        // user message whenever their adapter accepts that kind in user turns.
        (AdapterFamily::GoogleGemini, _) => (MediaDelivery::UserMessage, GEMINI_INLINE_BYTES),
        (AdapterFamily::GoogleVertexGemini, CapabilityMediaKind::Image) => {
            (MediaDelivery::UserMessage, VERTEX_IMAGE_BYTES)
        }
        (
            AdapterFamily::OpenaiChat
            | AdapterFamily::OpenaiCompatible
            | AdapterFamily::AzureOpenaiChat
            | AdapterFamily::CohereV2Chat,
            CapabilityMediaKind::Image,
        )
        | (
            AdapterFamily::OpenaiChat
            | AdapterFamily::OpenaiCompatible
            | AdapterFamily::GoogleVertexGemini,
            CapabilityMediaKind::Pdf,
        )
        | (AdapterFamily::GoogleVertexGemini, CapabilityMediaKind::Audio) => {
            (MediaDelivery::UserMessage, MAX_ENCODED_BYTES as u64)
        }
        (
            AdapterFamily::OpenaiCompatible
            | AdapterFamily::AnthropicCompatible
            | AdapterFamily::GoogleVertexGemini,
            CapabilityMediaKind::Video,
        ) => (MediaDelivery::UserMessage, MAX_VIDEO_ENCODED_BYTES as u64),
        _ => return None,
    })
}

/// How a tool's attachment of `mime_type` and `size` bytes reaches the
/// turn's model, whose accepted media are `input_media`.
#[must_use]
pub fn gate_attachment(
    input_media: &[ToolInputMedia],
    mime_type: &str,
    size: u64,
) -> AttachmentGate {
    let Some(kind) = media_kind(mime_type) else {
        return AttachmentGate::RejectUnsupportedModel;
    };
    let Some(accepted) = input_media.iter().find(|media| media.kind.as_str() == kind) else {
        return AttachmentGate::RejectUnsupportedModel;
    };
    let listed = accepted.mime_types.iter().any(|accepted| {
        if kind == MediaKindLabel::VIDEO {
            canonical_video_mime_type(accepted) == canonical_video_mime_type(mime_type)
        } else {
            accepted == mime_type
        }
    });
    if !listed {
        return AttachmentGate::RejectUnsupportedModel;
    }
    if size > accepted.max_bytes {
        return AttachmentGate::RejectTooLarge {
            max_bytes: accepted.max_bytes,
        };
    }
    match accepted.delivery {
        MediaDelivery::ToolResult => AttachmentGate::AttachToolResult,
        MediaDelivery::UserMessage => AttachmentGate::DeliverViaUserTurn,
        MediaDelivery::Unknown => AttachmentGate::RejectUnsupportedFamily,
    }
}

/// Leading bytes that [`sniff_media_type`] needs to recognize every format.
pub const MEDIA_SNIFF_BYTES: usize = 16 * 1024;

/// Recognizes a media file from its first [`MEDIA_SNIFF_BYTES`] (or all of
/// it, when shorter) and its total length, so size limits apply before the
/// whole file is read. A recognized file still needs [`approved_media_type`].
pub fn sniff_media_type(
    path: &Path,
    head: &[u8],
    len: u64,
) -> Result<Option<&'static str>, ToolError> {
    Ok(sniff(path, head, len)?.map(|(mime, _)| mime))
}

/// Recognizes and fully validates a media file.
pub fn approved_media_type(path: &Path, bytes: &[u8]) -> Result<Option<&'static str>, ToolError> {
    let Some((mime, kind)) = sniff(path, bytes, bytes.len() as u64)? else {
        return Ok(None);
    };
    let valid = match kind {
        MediaKind::Image(container) => validate_image(bytes, container),
        MediaKind::Pdf => validate_pdf(bytes),
        MediaKind::Audio => true,
        MediaKind::Video => true,
    };
    if valid {
        Ok(Some(mime))
    } else {
        Err(malformed_media())
    }
}

fn sniff(
    path: &Path,
    bytes: &[u8],
    len: u64,
) -> Result<Option<(&'static str, MediaKind)>, ToolError> {
    let known_extension = known_media_extension(path);
    let candidate = recognize(path, bytes, len);
    let Some((mime, kind)) = candidate else {
        return if known_extension {
            Err(malformed_media())
        } else {
            Ok(None)
        };
    };
    if known_extension && !media_extension_matches(path, mime) {
        return Err(malformed_media());
    }
    let max_encoded_bytes = match kind {
        MediaKind::Video => MAX_VIDEO_ENCODED_BYTES,
        MediaKind::Image(_) | MediaKind::Pdf | MediaKind::Audio => MAX_ENCODED_BYTES,
    };
    if len > max_encoded_bytes as u64 {
        return Err(ToolError::resource_limit(format!(
            "attachment is {len} bytes; the absolute validation cap is {max_encoded_bytes} bytes"
        )));
    }
    Ok(Some((mime, kind)))
}

fn malformed_media() -> ToolError {
    ToolError::execution("file extension identifies malformed image, PDF, audio, or video content")
}

/// Checks that `bytes` are a supported media file of the declared
/// `mime_type` within the attachment size bounds.
pub fn validate_attachment(mime_type: &str, path: &Path, bytes: &[u8]) -> Result<(), ToolError> {
    let max_attachment_bytes = if mime_type.starts_with("video/") {
        MAX_VIDEO_ATTACHMENT_BYTES
    } else {
        MAX_ATTACHMENT_BYTES
    };
    if bytes.len() as u64 > max_attachment_bytes {
        return Err(ToolError::resource_limit(format!(
            "attachment is {} bytes; the limit is {max_attachment_bytes} bytes",
            bytes.len()
        )));
    }
    let validated = approved_media_type(path, bytes)?.ok_or_else(|| {
        ToolError::execution("attachment is not a supported image, PDF, or video")
    })?;
    if canonical_video_mime_type(validated) != canonical_video_mime_type(mime_type) {
        return Err(ToolError::execution(format!(
            "attachment MIME mismatch: declared {mime_type}, validated {validated}"
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::{collections::BTreeMap, path::Path};

    use cookie_agent_protocol::{
        AdaptorId, CancellationCapability, MediaCapability, MediaKind, MimeType, Modality,
        ModelCapabilities, ReplayCapability, runtime_protocol::MediaDelivery,
    };

    use super::{
        AttachmentGate, BEDROCK_IMAGE_BYTES, BEDROCK_PDF_BYTES, MAX_VIDEO_ATTACHMENT_BYTES,
        UploadRejection, ValidatedUpload, approved_media_type, attachment_gate_error,
        tool_input_media, user_input_media, validate_attachment, validate_upload,
    };
    use crate::AdapterFamily;

    /// The gate of an attachment for a model of `family` with `capabilities`.
    fn gate_attachment(
        family: AdapterFamily,
        capabilities: &ModelCapabilities,
        mime_type: &str,
        size: u64,
    ) -> AttachmentGate {
        super::gate_attachment(&tool_input_media(family, capabilities), mime_type, size)
    }

    fn capabilities(kind: Option<(MediaKind, &str, u64)>) -> ModelCapabilities {
        let mut media = BTreeMap::new();
        let mut input = std::collections::BTreeSet::from([Modality::Text]);
        if let Some((kind, mime_type, max_bytes)) = kind {
            let modality = match kind {
                MediaKind::Image => Modality::Image,
                MediaKind::Audio => Modality::Audio,
                MediaKind::Pdf => Modality::Pdf,
                MediaKind::Video => Modality::Video,
            };
            input.insert(modality);
            media.insert(
                kind,
                MediaCapability {
                    mime_types: [MimeType::new(mime_type).unwrap()].into_iter().collect(),
                    max_bytes,
                    max_count: 1,
                },
            );
        }
        ModelCapabilities {
            input,
            output: [Modality::Text].into_iter().collect(),
            context_tokens: 8_192,
            output_tokens: 2_048,
            tool_calling: true,
            parallel_tool_calls: false,
            structured_output: false,
            reasoning: false,
            temperature: true,
            top_p: true,
            seed: false,
            native_replay: ReplayCapability::Optional,
            cancellation: CancellationCapability::LocalOnly,
            media,
        }
    }

    fn ftyp(brand: &[u8; 4]) -> Vec<u8> {
        let mut bytes = 16_u32.to_be_bytes().to_vec();
        bytes.extend_from_slice(b"ftyp");
        bytes.extend_from_slice(brand);
        bytes.extend_from_slice(&[0; 4]);
        bytes
    }

    fn ftyp_with_compatible_brand(major: &[u8; 4], compatible: &[u8; 4]) -> Vec<u8> {
        let mut bytes = 20_u32.to_be_bytes().to_vec();
        bytes.extend_from_slice(b"ftyp");
        bytes.extend_from_slice(major);
        bytes.extend_from_slice(&[0; 4]);
        bytes.extend_from_slice(compatible);
        bytes
    }

    fn large_ftyp(major: &[u8; 4], compatible: &[u8; 4]) -> Vec<u8> {
        let mut bytes = 1_u32.to_be_bytes().to_vec();
        bytes.extend_from_slice(b"ftyp");
        bytes.extend_from_slice(&28_u64.to_be_bytes());
        bytes.extend_from_slice(major);
        bytes.extend_from_slice(&[0; 4]);
        bytes.extend_from_slice(compatible);
        bytes
    }

    fn png_chunk(bytes: &mut Vec<u8>, kind: &[u8; 4], data: &[u8]) {
        bytes.extend_from_slice(&u32::try_from(data.len()).unwrap().to_be_bytes());
        bytes.extend_from_slice(kind);
        bytes.extend_from_slice(data);
        let mut crc = crc32fast::Hasher::new();
        crc.update(kind);
        crc.update(data);
        bytes.extend_from_slice(&crc.finalize().to_be_bytes());
    }

    /// A PNG whose IDAT is not a valid zlib stream: validation never decodes pixel data.
    fn png(width: u32, height: u32) -> Vec<u8> {
        let mut bytes = b"\x89PNG\r\n\x1a\n".to_vec();
        let mut header = width.to_be_bytes().to_vec();
        header.extend_from_slice(&height.to_be_bytes());
        header.extend_from_slice(&[8, 6, 0, 0, 0]);
        png_chunk(&mut bytes, b"IHDR", &header);
        png_chunk(&mut bytes, b"IDAT", b"not zlib");
        png_chunk(&mut bytes, b"IEND", b"");
        bytes
    }

    fn jpeg(width: u16, height: u16) -> Vec<u8> {
        let mut bytes = vec![0xff, 0xd8, 0xff, 0xc0, 0x00, 0x0b, 0x08];
        bytes.extend_from_slice(&height.to_be_bytes());
        bytes.extend_from_slice(&width.to_be_bytes());
        bytes.extend_from_slice(&[0x01, 0x01, 0x11, 0x00]);
        bytes.extend_from_slice(&[0xff, 0xda, 0x00, 0x08, 0x01, 0x01, 0x00, 0x00, 0x3f, 0x00]);
        bytes.extend_from_slice(&[0x12, 0xff, 0x00, 0x34, 0xff, 0xd9]);
        bytes
    }

    fn riff_webp(chunks: &[(&[u8; 4], Vec<u8>)]) -> Vec<u8> {
        let mut body = b"WEBP".to_vec();
        for (kind, data) in chunks {
            body.extend_from_slice(*kind);
            body.extend_from_slice(&u32::try_from(data.len()).unwrap().to_le_bytes());
            body.extend_from_slice(data);
            if data.len() % 2 == 1 {
                body.push(0);
            }
        }
        let mut bytes = b"RIFF".to_vec();
        bytes.extend_from_slice(&u32::try_from(body.len()).unwrap().to_le_bytes());
        bytes.extend_from_slice(&body);
        bytes
    }

    fn vp8l(width: u32, height: u32) -> (&'static [u8; 4], Vec<u8>) {
        let bits = (width - 1) | ((height - 1) << 14);
        let mut data = vec![0x2f];
        data.extend_from_slice(&bits.to_le_bytes());
        (b"VP8L", data)
    }

    fn vp8x(width: u32, height: u32, flags: u8) -> (&'static [u8; 4], Vec<u8>) {
        let mut data = vec![flags, 0, 0, 0];
        data.extend_from_slice(&(width - 1).to_le_bytes()[..3]);
        data.extend_from_slice(&(height - 1).to_le_bytes()[..3]);
        (b"VP8X", data)
    }

    fn gif(width: u16, height: u16, frames: usize) -> Vec<u8> {
        let mut bytes = b"GIF89a".to_vec();
        bytes.extend_from_slice(&width.to_le_bytes());
        bytes.extend_from_slice(&height.to_le_bytes());
        bytes.extend_from_slice(&[0, 0, 0]);
        for _ in 0..frames {
            bytes.extend_from_slice(&[0x2c, 0, 0, 0, 0]);
            bytes.extend_from_slice(&width.to_le_bytes());
            bytes.extend_from_slice(&height.to_le_bytes());
            bytes.extend_from_slice(&[0, 2, 1, 0, 0]);
        }
        bytes.push(0x3b);
        bytes
    }

    fn accepts(name: &str, bytes: &[u8]) -> Option<&'static str> {
        approved_media_type(Path::new(name), bytes).unwrap()
    }

    fn rejects(name: &str, bytes: &[u8]) -> bool {
        approved_media_type(Path::new(name), bytes).is_err()
    }

    #[test]
    fn images_are_validated_from_container_structure_without_decoding() {
        assert_eq!(accepts("a.png", &png(3, 2)), Some("image/png"));
        assert_eq!(accepts("a.jpg", &jpeg(3, 2)), Some("image/jpeg"));
        assert_eq!(
            accepts("a.webp", &riff_webp(&[vp8l(3, 2)])),
            Some("image/webp")
        );
        assert_eq!(
            accepts("a.webp", &riff_webp(&[vp8x(3, 2, 0x10), vp8l(3, 2)])),
            Some("image/webp")
        );
        let mut lossy = vec![0x00, 0x00, 0x00, 0x9d, 0x01, 0x2a];
        lossy.extend_from_slice(&3_u16.to_le_bytes());
        lossy.extend_from_slice(&2_u16.to_le_bytes());
        assert_eq!(
            accepts("a.webp", &riff_webp(&[(b"VP8 ", lossy)])),
            Some("image/webp")
        );
        assert_eq!(accepts("a.gif", &gif(3, 2, 2)), Some("image/gif"));
    }

    #[test]
    fn truncated_or_corrupt_image_containers_are_rejected() {
        let valid = png(3, 2);
        assert!(rejects("a.png", &valid[..valid.len() - 1]));
        let mut corrupt = valid.clone();
        corrupt[20] ^= 1;
        assert!(rejects("a.png", &corrupt), "IHDR CRC mismatch");
        let mut no_idat = b"\x89PNG\r\n\x1a\n".to_vec();
        no_idat.extend_from_slice(&valid[8..33]);
        png_chunk(&mut no_idat, b"IEND", b"");
        assert!(rejects("a.png", &no_idat));
        let mut bad_depth = b"\x89PNG\r\n\x1a\n".to_vec();
        png_chunk(
            &mut bad_depth,
            b"IHDR",
            &[0, 0, 0, 3, 0, 0, 0, 2, 4, 2, 0, 0, 0],
        );
        png_chunk(&mut bad_depth, b"IDAT", b"x");
        png_chunk(&mut bad_depth, b"IEND", b"");
        assert!(rejects("a.png", &bad_depth), "RGB at 4 bits per sample");

        let valid = jpeg(3, 2);
        assert!(rejects("a.jpg", &valid[..valid.len() - 2]), "missing EOI");
        let mut no_frame = vec![0xff, 0xd8];
        no_frame.extend_from_slice(&valid[15..]);
        assert!(rejects("a.jpg", &no_frame), "scan before a frame header");

        assert!(rejects(
            "a.webp",
            &riff_webp(&[vp8x(3, 2, 0x02), vp8l(3, 2)])
        ));
        assert!(rejects("a.webp", &riff_webp(&[vp8l(3, 2), vp8l(3, 2)])));
        assert!(rejects(
            "a.webp",
            &riff_webp(&[(b"VP8L", vec![0x2e, 0, 0, 0, 0])])
        ));

        let valid = gif(3, 2, 1);
        assert!(rejects("a.gif", &valid[..valid.len() - 1]));
        assert!(rejects("a.gif", &gif(3, 2, 0)));
    }

    #[test]
    fn image_dimension_and_pixel_limits_are_header_math() {
        assert!(accepts("a.png", &png(16_384, 1_024)).is_some());
        assert!(rejects("a.png", &png(16_385, 1)));
        assert!(rejects("a.png", &png(16_384, 1_025)));
        assert!(rejects("a.png", &png(3, 0)));
        assert!(rejects("a.jpg", &jpeg(3, 0)));
        assert!(rejects("a.jpg", &jpeg(16_385, 1)));
        assert!(rejects(
            "a.webp",
            &riff_webp(&[vp8x(16_385, 1, 0), vp8l(1, 1)])
        ));
        assert!(accepts("a.gif", &gif(4_096, 4_096, 1)).is_some());
        assert!(
            rejects("a.gif", &gif(4_096, 4_096, 2)),
            "frames composite onto the full screen"
        );
        assert!(accepts("a.gif", &gif(1, 1, 256)).is_some());
        assert!(rejects("a.gif", &gif(1, 1, 257)));
    }

    fn pdf(xref_stream: bool, trailer_extra: &str) -> Vec<u8> {
        let mut bytes = b"%PDF-1.7\n".to_vec();
        let mut offsets = Vec::new();
        for object in [
            "1 0 obj\n<< /Type /Catalog /Pages 2 0 R >>\nendobj\n",
            "2 0 obj\n<< /Type /Pages /Kids [3 0 R] /Count 1 >>\nendobj\n",
            "3 0 obj\n<< /Type /Page /Parent 2 0 R /MediaBox [0 0 1 1] >>\nendobj\n",
        ] {
            offsets.push(bytes.len());
            bytes.extend_from_slice(object.as_bytes());
        }
        let xref = bytes.len();
        if xref_stream {
            bytes.extend_from_slice(
                format!(
                    "4 0 obj\n<< /Type /XRef /Size 5 /W [1 2 1] /Root 1 0 R{trailer_extra} /Length 0 >>\nstream\n\nendstream\nendobj\n"
                )
                .as_bytes(),
            );
        } else {
            bytes.extend_from_slice(b"xref\n0 4\n0000000000 65535 f \n");
            for offset in offsets {
                bytes.extend_from_slice(format!("{offset:010} 00000 n \n").as_bytes());
            }
            bytes.extend_from_slice(
                format!("trailer\n<< /Size 4 /Root 1 0 R{trailer_extra} >>\n").as_bytes(),
            );
        }
        bytes.extend_from_slice(format!("startxref\n{xref}\n%%EOF\n").as_bytes());
        bytes
    }

    #[test]
    fn pdfs_need_a_header_eof_marker_and_resolvable_startxref() {
        assert_eq!(accepts("a.pdf", &pdf(false, "")), Some("application/pdf"));
        assert_eq!(
            accepts("a.pdf", &pdf(true, "")),
            Some("application/pdf"),
            "cross-reference streams (PDF 1.5+) are accepted"
        );
        let mut junk = pdf(false, "");
        junk.extend_from_slice(&[b' '; 900]);
        assert!(accepts("a.pdf", &junk).is_some(), "trailing junk near EOF");

        assert!(rejects("a.pdf", b"not a PDF"));
        assert!(rejects("a.pdf", b"%PDF-9.0\n%%EOF\n"));
        let valid = pdf(false, "");
        assert!(
            rejects("a.pdf", &valid[..valid.len() - 7]),
            "truncated before %%EOF"
        );
        let mut far_junk = valid.clone();
        far_junk.extend_from_slice(&[b' '; 2_048]);
        assert!(rejects("a.pdf", &far_junk), "%%EOF must be near the end");
        let text = String::from_utf8(valid.clone()).unwrap();
        let without_startxref = text.replace("startxref", "startxrex");
        assert!(rejects("a.pdf", without_startxref.as_bytes()));
        let xref = text.find("xref\n0 4").unwrap();
        let out_of_range = text.replace(
            &format!("startxref\n{xref}\n"),
            &format!("startxref\n{}\n", valid.len()),
        );
        assert!(rejects("a.pdf", out_of_range.as_bytes()));
        let misaligned = text.replace(
            &format!("startxref\n{xref}\n"),
            &format!("startxref\n{}\n", xref + 1),
        );
        assert!(rejects("a.pdf", misaligned.as_bytes()));
    }

    #[test]
    fn encrypted_pdfs_are_rejected() {
        assert!(rejects("a.pdf", &pdf(false, " /Encrypt 5 0 R")));
        assert!(rejects("a.pdf", &pdf(true, " /Encrypt 5 0 R")));
    }

    #[test]
    fn absolute_media_cap_is_named_in_resource_limit_errors() {
        let mut video = b"FLV\x01".to_vec();
        video.resize(super::MAX_VIDEO_ENCODED_BYTES + 1, 0);
        let error = approved_media_type(Path::new("clip.flv"), &video)
            .expect_err("oversized media must fail the absolute cap");
        assert!(error.message().contains("absolute validation cap"));
    }

    #[test]
    fn attachment_gate_is_exhaustive_across_adapters_media_and_capability() {
        use AttachmentGate::{AttachToolResult as Tool, DeliverViaUserTurn as User};
        // A kind the family cannot deliver is not among the model's input
        // media at all.
        let reject = AttachmentGate::RejectUnsupportedModel;
        let families = [
            (AdapterFamily::Anthropic, Tool, Tool),
            (AdapterFamily::AnthropicCompatible, Tool, Tool),
            (AdapterFamily::OpenaiChat, User, User),
            (AdapterFamily::OpenaiResponses, Tool, Tool),
            (AdapterFamily::OpenaiCompatible, User, User),
            (AdapterFamily::GoogleGemini, User, User),
            (AdapterFamily::GoogleVertexGemini, User, User),
            (AdapterFamily::AwsBedrockConverse, Tool, Tool),
            (AdapterFamily::AzureOpenaiChat, User, reject),
            (AdapterFamily::AzureOpenaiResponses, Tool, Tool),
            (AdapterFamily::CohereV2Chat, User, reject),
        ];
        for (family, image_delivery, pdf_delivery) in families {
            for (kind, mime_type, delivery) in [
                (MediaKind::Image, "image/png", image_delivery),
                (MediaKind::Pdf, "application/pdf", pdf_delivery),
            ] {
                assert_eq!(
                    gate_attachment(family, &capabilities(None), mime_type, 5),
                    AttachmentGate::RejectUnsupportedModel,
                    "{family:?} {kind:?} without capability"
                );
                assert_eq!(
                    gate_attachment(
                        family,
                        &capabilities(Some((kind, mime_type, 20 * 1024 * 1024))),
                        mime_type,
                        5,
                    ),
                    delivery,
                    "{family:?} {kind:?} with capability"
                );
            }
        }
    }

    #[test]
    fn attachment_gate_canonicalizes_video_aliases_and_applies_size_clamps() {
        for (advertised, observed) in [
            ("video/mov", "video/quicktime"),
            ("video/avi", "video/x-msvideo"),
            ("video/mpg", "video/mpeg"),
        ] {
            assert_eq!(
                gate_attachment(
                    AdapterFamily::OpenaiCompatible,
                    &capabilities(Some((MediaKind::Video, advertised, 1024))),
                    observed,
                    5,
                ),
                AttachmentGate::DeliverViaUserTurn
            );
        }
        let image = capabilities(Some((MediaKind::Image, "image/png", u64::MAX)));
        let pdf = capabilities(Some((MediaKind::Pdf, "application/pdf", u64::MAX)));
        assert_eq!(
            gate_attachment(
                AdapterFamily::AwsBedrockConverse,
                &image,
                "image/png",
                BEDROCK_IMAGE_BYTES + 1,
            ),
            AttachmentGate::RejectTooLarge {
                max_bytes: BEDROCK_IMAGE_BYTES
            }
        );
        assert_eq!(
            gate_attachment(
                AdapterFamily::AwsBedrockConverse,
                &pdf,
                "application/pdf",
                BEDROCK_PDF_BYTES + 1,
            ),
            AttachmentGate::RejectTooLarge {
                max_bytes: BEDROCK_PDF_BYTES
            }
        );
        assert_eq!(
            attachment_gate_error(
                AttachmentGate::RejectTooLarge {
                    max_bytes: BEDROCK_IMAGE_BYTES,
                },
                "image/png",
                &"test/model".parse().unwrap(),
                &AdaptorId::AWS_BEDROCK_CONVERSE,
            )
            .unwrap(),
            "Cannot attach image/png: exceeds the 3.75 MiB inline limit for this provider"
        );
    }

    #[test]
    fn tool_input_media_lists_what_the_family_delivers_with_effective_limits() {
        let mut model = capabilities(Some((MediaKind::Image, "image/png", u64::MAX)));
        model.media.insert(
            MediaKind::Video,
            MediaCapability {
                mime_types: [MimeType::new("video/mp4").unwrap()].into_iter().collect(),
                max_bytes: 1024,
                max_count: 1,
            },
        );
        let media = tool_input_media(AdapterFamily::Anthropic, &model);
        // Anthropic carries images in tool results and no video at all.
        assert_eq!(media.len(), 1);
        assert_eq!(media[0].kind.as_str(), "image");
        assert_eq!(media[0].mime_types, ["image/png"]);
        assert_eq!(media[0].max_bytes, super::ANTHROPIC_IMAGE_BYTES);
        assert_eq!(media[0].delivery, MediaDelivery::ToolResult);

        let media = tool_input_media(AdapterFamily::AnthropicCompatible, &model);
        assert_eq!(media.len(), 2);
        assert_eq!(media[1].kind.as_str(), "video");
        assert_eq!(media[1].max_bytes, 1024, "the model's lower limit wins");
        assert_eq!(media[1].delivery, MediaDelivery::UserMessage);
        assert!(tool_input_media(AdapterFamily::Anthropic, &capabilities(None)).is_empty());
    }

    fn with_media(
        mut model: ModelCapabilities,
        kind: MediaKind,
        mime_types: &[&str],
        max_bytes: u64,
        max_count: u32,
    ) -> ModelCapabilities {
        let modality = match kind {
            MediaKind::Image => Modality::Image,
            MediaKind::Audio => Modality::Audio,
            MediaKind::Pdf => Modality::Pdf,
            MediaKind::Video => Modality::Video,
        };
        model.input.insert(modality);
        model.media.insert(
            kind,
            MediaCapability {
                mime_types: mime_types
                    .iter()
                    .map(|mime_type| MimeType::new(*mime_type).unwrap())
                    .collect(),
                max_bytes,
                max_count,
            },
        );
        model
    }

    /// `(kind, mime types, max bytes, max dimension, max pixels, max count)`.
    type MediaSummary = (String, Vec<String>, u64, Option<u32>, Option<u64>, u32);

    fn summary(family: AdapterFamily, model: &ModelCapabilities) -> Vec<MediaSummary> {
        user_input_media(family, model)
            .into_iter()
            .map(|media| {
                (
                    media.kind.as_str().to_owned(),
                    media.mime_types,
                    media.max_bytes,
                    media.max_dimension,
                    media.max_pixels,
                    media.max_count,
                )
            })
            .collect()
    }

    #[test]
    fn user_input_media_takes_the_lowest_of_the_model_the_family_and_the_attachment_caps() {
        let model = with_media(
            with_media(
                with_media(
                    capabilities(None),
                    MediaKind::Image,
                    &["image/png", "image/jpeg", "image/heic"],
                    30 * 1024 * 1024,
                    20,
                ),
                MediaKind::Pdf,
                &["application/pdf"],
                30 * 1024 * 1024,
                5,
            ),
            MediaKind::Video,
            &["video/mov", "video/mp4"],
            30 * 1024 * 1024,
            1,
        );
        let image = |max_bytes| {
            (
                "image".to_owned(),
                vec!["image/jpeg".to_owned(), "image/png".to_owned()],
                max_bytes,
                Some(16_384),
                Some(16_777_216),
                20,
            )
        };
        let pdf = |max_bytes| {
            (
                "pdf".to_owned(),
                vec!["application/pdf".to_owned()],
                max_bytes,
                None,
                None,
                5,
            )
        };
        let video = |max_bytes| {
            (
                "video".to_owned(),
                vec!["video/mp4".to_owned(), "video/quicktime".to_owned()],
                max_bytes,
                None,
                None,
                1,
            )
        };
        // Anthropic: images at its 7.5 MiB inline limit, PDFs at the
        // attachment cap, and no video at all.
        assert_eq!(
            summary(AdapterFamily::Anthropic, &model),
            [image(7_864_320), pdf(20_971_520)]
        );
        // OpenAI Responses carries images and PDFs in tool results; in a
        // user turn they count the same way.
        assert_eq!(
            summary(AdapterFamily::OpenaiResponses, &model),
            [image(20_971_520), pdf(20_971_520)]
        );
        // OpenAI Chat carries none of them in tool results, yet accepts
        // images and PDFs in a user turn: they are offered.
        assert_eq!(
            summary(AdapterFamily::OpenaiChat, &model),
            [image(20_971_520), pdf(20_971_520)]
        );
        // Bedrock's video budget is below the 25 MiB video cap.
        assert_eq!(
            summary(AdapterFamily::AwsBedrockConverse, &model),
            [image(3_932_160), pdf(4_718_592), video(19_660_797)]
        );
        // An Anthropic-compatible endpoint takes video up to the video cap.
        assert_eq!(
            summary(AdapterFamily::AnthropicCompatible, &model),
            [image(7_864_320), pdf(20_971_520), video(26_214_400)]
        );
        // Gemini's inline request budget bounds every kind.
        assert_eq!(
            summary(AdapterFamily::GoogleGemini, &model),
            [image(14_680_064), pdf(14_680_064), video(14_680_064)]
        );
        // The model's own lower limit wins.
        let small = with_media(capabilities(None), MediaKind::Image, &["image/*"], 1_000, 3);
        assert_eq!(
            summary(AdapterFamily::OpenaiChat, &small),
            [(
                "image".to_owned(),
                vec![
                    "image/gif".to_owned(),
                    "image/jpeg".to_owned(),
                    "image/png".to_owned(),
                    "image/webp".to_owned()
                ],
                1_000,
                Some(16_384),
                Some(16_777_216),
                3
            )]
        );
        // A kind no upload can be recognized as is not offered.
        let heic = with_media(
            capabilities(None),
            MediaKind::Image,
            &["image/heic"],
            1_000,
            1,
        );
        assert!(user_input_media(AdapterFamily::OpenaiChat, &heic).is_empty());
        assert!(user_input_media(AdapterFamily::Anthropic, &capabilities(None)).is_empty());
    }

    #[test]
    fn uploads_are_recognized_from_their_content_and_report_image_sizes() {
        assert_eq!(
            validate_upload(&png(640, 480)),
            Ok(ValidatedUpload {
                media_type: "image/png",
                kind: "image",
                image: Some((640, 480)),
            })
        );
        assert_eq!(
            validate_upload(&jpeg(32, 16)).map(|upload| upload.image),
            Ok(Some((32, 16)))
        );
        assert_eq!(
            validate_upload(&gif(8, 4, 2)).map(|upload| upload.image),
            Ok(Some((8, 4)))
        );
        assert_eq!(
            validate_upload(b"plain text").unwrap_err(),
            UploadRejection::Unrecognized
        );
        let mut torn = png(4, 4);
        torn.truncate(torn.len() - 3);
        assert_eq!(
            validate_upload(&torn).unwrap_err(),
            UploadRejection::Malformed {
                media_type: "image/png"
            }
        );
        assert_eq!(
            validate_upload(&png(20_000, 10)).unwrap_err(),
            UploadRejection::TooLarge {
                kind: "image",
                byte_length: png(20_000, 10).len() as u64,
                max_bytes: 20 * 1024 * 1024,
                image: Some((20_000, 10)),
            }
        );
        let mut huge = png(4, 4);
        huge.resize(20 * 1024 * 1024 + 1, 0);
        assert_eq!(
            validate_upload(&huge).unwrap_err(),
            UploadRejection::TooLarge {
                kind: "image",
                byte_length: 20 * 1024 * 1024 + 1,
                max_bytes: 20 * 1024 * 1024,
                image: None,
            }
        );
    }

    #[test]
    fn bedrock_video_attaches_within_the_raw_byte_budget() {
        let video = capabilities(Some((MediaKind::Video, "video/mp4", u64::MAX)));
        assert_eq!(
            gate_attachment(AdapterFamily::AwsBedrockConverse, &video, "video/mp4", 5,),
            AttachmentGate::AttachToolResult
        );
        assert_eq!(
            gate_attachment(
                AdapterFamily::AwsBedrockConverse,
                &video,
                "video/mp4",
                super::BEDROCK_VIDEO_BYTES,
            ),
            AttachmentGate::AttachToolResult
        );
        assert_eq!(
            gate_attachment(
                AdapterFamily::AwsBedrockConverse,
                &video,
                "video/mp4",
                super::BEDROCK_VIDEO_BYTES + 1,
            ),
            AttachmentGate::RejectTooLarge {
                max_bytes: super::BEDROCK_VIDEO_BYTES
            }
        );
        // The constant honors the strict base64 bound: 4 * ceil(n / 3) < 25 MiB.
        let encoded = 4 * super::BEDROCK_VIDEO_BYTES.div_ceil(3);
        assert!(encoded < 25 * 1024 * 1024);
        let encoded_over = 4 * (super::BEDROCK_VIDEO_BYTES + 1).div_ceil(3);
        assert!(encoded_over >= 25 * 1024 * 1024);
        // Catalog size precedes family deliverability: an attachment that exceeds
        // the model's advertised limit reports the size, not the family.
        let small_cap = capabilities(Some((MediaKind::Pdf, "application/pdf", 8)));
        assert_eq!(
            gate_attachment(AdapterFamily::OpenaiChat, &small_cap, "application/pdf", 16,),
            AttachmentGate::RejectTooLarge { max_bytes: 8 }
        );
    }

    #[test]
    fn provider_inline_limits_clamp_images_and_gemini_requests() {
        let image = capabilities(Some((MediaKind::Image, "image/png", u64::MAX)));
        let video = capabilities(Some((MediaKind::Video, "video/mp4", u64::MAX)));
        for (family, capabilities, mime_type, max_bytes) in [
            (
                AdapterFamily::Anthropic,
                &image,
                "image/png",
                super::ANTHROPIC_IMAGE_BYTES,
            ),
            (
                AdapterFamily::AnthropicCompatible,
                &image,
                "image/png",
                super::ANTHROPIC_IMAGE_BYTES,
            ),
            (
                AdapterFamily::GoogleVertexGemini,
                &image,
                "image/png",
                super::VERTEX_IMAGE_BYTES,
            ),
            (
                AdapterFamily::GoogleGemini,
                &image,
                "image/png",
                super::GEMINI_INLINE_BYTES,
            ),
            (
                AdapterFamily::GoogleGemini,
                &video,
                "video/mp4",
                super::GEMINI_INLINE_BYTES,
            ),
        ] {
            assert_ne!(
                gate_attachment(family, capabilities, mime_type, max_bytes),
                AttachmentGate::RejectTooLarge { max_bytes },
                "{family:?} {mime_type} at the limit"
            );
            assert_eq!(
                gate_attachment(family, capabilities, mime_type, max_bytes + 1),
                AttachmentGate::RejectTooLarge { max_bytes },
                "{family:?} {mime_type} over the limit"
            );
        }
        // The Anthropic raw limit is the largest whose base64 fits in 10 MiB.
        assert_eq!(
            4 * super::ANTHROPIC_IMAGE_BYTES.div_ceil(3),
            10 * 1024 * 1024
        );
        assert!(4 * super::GEMINI_INLINE_BYTES.div_ceil(3) < 20 * 1024 * 1024);
    }

    #[test]
    fn user_turn_video_families_use_the_emitted_delivery_channel() {
        let video = capabilities(Some((MediaKind::Video, "video/mp4", 1024)));
        for family in [
            AdapterFamily::OpenaiCompatible,
            AdapterFamily::AnthropicCompatible,
            AdapterFamily::GoogleGemini,
            AdapterFamily::GoogleVertexGemini,
        ] {
            assert_eq!(
                gate_attachment(family, &video, "video/mp4", 5),
                AttachmentGate::DeliverViaUserTurn,
                "{family:?}"
            );
        }
        assert_eq!(
            gate_attachment(AdapterFamily::AwsBedrockConverse, &video, "video/mp4", 5),
            AttachmentGate::AttachToolResult
        );
        assert_eq!(
            gate_attachment(
                AdapterFamily::OpenaiCompatible,
                &capabilities(None),
                "video/mp4",
                5
            ),
            AttachmentGate::RejectUnsupportedModel
        );
        assert_eq!(
            gate_attachment(AdapterFamily::Anthropic, &video, "video/mp4", 5),
            AttachmentGate::RejectUnsupportedModel
        );
        // A delivery this build does not know is one a tool cannot use.
        let mut unknown = tool_input_media(AdapterFamily::OpenaiCompatible, &video);
        unknown[0].delivery = MediaDelivery::Unknown;
        assert_eq!(
            super::gate_attachment(&unknown, "video/mp4", 5),
            AttachmentGate::RejectUnsupportedFamily
        );
    }

    #[test]
    fn audio_signatures_are_sniffed_and_gemini_uses_user_turn_delivery() {
        for (path, bytes, mime) in [
            ("clip.mp3", b"ID3payload".as_slice(), "audio/mpeg"),
            ("clip.mp3", b"\xff\xfbpayload".as_slice(), "audio/mpeg"),
            (
                "clip.wav",
                b"RIFF\x04\x00\x00\x00WAVEpayload".as_slice(),
                "audio/wav",
            ),
            ("clip.ogg", b"OggSpayload".as_slice(), "audio/ogg"),
            ("clip.flac", b"fLaCpayload".as_slice(), "audio/flac"),
        ] {
            assert_eq!(
                approved_media_type(Path::new(path), bytes).unwrap(),
                Some(mime),
                "{path}"
            );
        }

        let audio = capabilities(Some((MediaKind::Audio, "audio/mpeg", 1024)));
        for family in [
            AdapterFamily::GoogleGemini,
            AdapterFamily::GoogleVertexGemini,
        ] {
            assert_eq!(
                gate_attachment(family, &audio, "audio/mpeg", 10),
                AttachmentGate::DeliverViaUserTurn
            );
        }
    }

    #[test]
    fn iso_base_media_brands_identify_mp4_quicktime_and_3gpp() {
        assert_eq!(
            approved_media_type(Path::new("clip.mp4"), &ftyp(b"isom")).unwrap(),
            Some("video/mp4")
        );
        assert_eq!(
            approved_media_type(Path::new("clip.mov"), &ftyp(b"qt  ")).unwrap(),
            Some("video/quicktime")
        );
        assert_eq!(
            approved_media_type(Path::new("clip.3gp"), &ftyp(b"3gp6")).unwrap(),
            Some("video/3gpp")
        );
        for major in [b"MSNV", b"av01", b"mp71"] {
            assert_eq!(
                approved_media_type(
                    Path::new("clip.mp4"),
                    &ftyp_with_compatible_brand(major, b"mp42")
                )
                .unwrap(),
                Some("video/mp4")
            );
        }
        assert_eq!(
            approved_media_type(Path::new("clip.mp4"), &large_ftyp(b"MSNV", b"isom")).unwrap(),
            Some("video/mp4")
        );
    }

    #[test]
    fn ebml_identifies_webm_and_matroska_by_extension() {
        let bytes = [0x1a, 0x45, 0xdf, 0xa3];
        assert_eq!(
            approved_media_type(Path::new("clip.webm"), &bytes).unwrap(),
            Some("video/webm")
        );
        assert_eq!(
            approved_media_type(Path::new("clip.mkv"), &bytes).unwrap(),
            Some("video/x-matroska")
        );
    }

    #[test]
    fn riff_flv_and_mpeg_headers_are_recognized() {
        assert_eq!(
            approved_media_type(Path::new("clip.avi"), b"RIFF\x04\x00\x00\x00AVI ").unwrap(),
            Some("video/x-msvideo")
        );
        assert_eq!(
            approved_media_type(Path::new("clip.flv"), b"FLV\x01").unwrap(),
            Some("video/x-flv")
        );
        for signature in [[0x00, 0x00, 0x01, 0xba], [0x00, 0x00, 0x01, 0xb3]] {
            assert_eq!(
                approved_media_type(Path::new("clip.mpeg"), &signature).unwrap(),
                Some("video/mpeg")
            );
        }
    }

    #[test]
    fn asf_guid_identifies_wmv() {
        let guid = [
            0x30, 0x26, 0xb2, 0x75, 0x8e, 0x66, 0xcf, 0x11, 0xa6, 0xd9, 0x00, 0xaa, 0x00, 0x62,
            0xce, 0x6c,
        ];
        assert_eq!(
            approved_media_type(Path::new("clip.wmv"), &guid).unwrap(),
            Some("video/wmv")
        );
    }

    #[test]
    fn known_video_extensions_reject_malformed_and_mismatched_content() {
        assert!(approved_media_type(Path::new("clip.mp4"), b"not video").is_err());
        assert!(approved_media_type(Path::new("clip.mp4"), &[0x1a, 0x45, 0xdf, 0xa3]).is_err());
        assert_eq!(
            approved_media_type(Path::new("clip.bin"), b"not video").unwrap(),
            None
        );
    }

    #[test]
    fn video_attachment_limit_accepts_boundary_and_rejects_plus_one() {
        let video = |size: usize| {
            let mut bytes = vec![0_u8; size];
            bytes[..4].copy_from_slice(b"FLV\x01");
            bytes
        };
        let at_limit = video(MAX_VIDEO_ATTACHMENT_BYTES as usize);
        validate_attachment("video/x-flv", Path::new("clip.flv"), &at_limit).unwrap();

        let over_limit = video(MAX_VIDEO_ATTACHMENT_BYTES as usize + 1);
        let error =
            validate_attachment("video/x-flv", Path::new("clip.flv"), &over_limit).unwrap_err();
        assert!(error.to_string().contains("26214400"));
    }

    #[test]
    fn video_mime_aliases_validate_in_alias_and_canonical_forms() {
        let mut quicktime = 16_u32.to_be_bytes().to_vec();
        quicktime.extend_from_slice(b"ftypqt  \0\0\0\0");
        for declared in ["video/mov", "video/quicktime"] {
            validate_attachment(declared, Path::new("clip.mov"), &quicktime).unwrap();
        }

        let avi = b"RIFF\x04\x00\x00\x00AVI ";
        for declared in ["video/avi", "video/x-msvideo"] {
            validate_attachment(declared, Path::new("clip.avi"), avi).unwrap();
        }

        let mpeg = [0x00, 0x00, 0x01, 0xba];
        for declared in ["video/mpg", "video/mpeg"] {
            validate_attachment(declared, Path::new("clip.mpg"), &mpeg).unwrap();
        }
    }
}
