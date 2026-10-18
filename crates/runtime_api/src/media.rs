//! Tool attachments: recognizing and validating media files, and deciding
//! how a turn's model receives them.

use std::path::Path;

use crate::{AdapterFamily, ToolError};
use cookie_agent_protocol::{
    AdaptorId, MediaKind as CapabilityMediaKind, ModelCapabilities, ModelKey,
};

const MAX_ENCODED_BYTES: usize = 20 * 1024 * 1024;
/// Upper bound on the compatible-brand list scanned when the major brand is unrecognized.
const MAX_BRAND_SCAN_BYTES: usize = 4 * 1024;
const MAX_VIDEO_ENCODED_BYTES: usize = 25 * 1024 * 1024;
const MAX_IMAGE_DIMENSION: u32 = 16_384;
const MAX_IMAGE_PIXELS: u64 = 16 * 1024 * 1024;
const MAX_ANIMATION_FRAMES: usize = 256;
/// Window at the end of a PDF searched for `%%EOF`, and before it for `startxref`; readers
/// tolerate trailing junk within about this distance.
const PDF_TAIL_BYTES: usize = 1024;
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

#[must_use]
pub fn gate_attachment(
    family: AdapterFamily,
    capabilities: &ModelCapabilities,
    mime_type: &str,
    size: u64,
) -> AttachmentGate {
    let kind = if mime_type.starts_with("image/") {
        CapabilityMediaKind::Image
    } else if mime_type == "application/pdf" {
        CapabilityMediaKind::Pdf
    } else if mime_type.starts_with("audio/") {
        CapabilityMediaKind::Audio
    } else if mime_type.starts_with("video/") {
        CapabilityMediaKind::Video
    } else {
        return AttachmentGate::RejectUnsupportedModel;
    };
    let Some(capability) = capabilities.media.get(&kind) else {
        return AttachmentGate::RejectUnsupportedModel;
    };
    let accepted = capability.mime_types.iter().any(|accepted| {
        if kind == CapabilityMediaKind::Video {
            canonical_video_mime_type(accepted.as_str()) == canonical_video_mime_type(mime_type)
        } else {
            accepted.as_str() == mime_type
        }
    });
    if !accepted {
        return AttachmentGate::RejectUnsupportedModel;
    }
    if size > capability.max_bytes {
        return AttachmentGate::RejectTooLarge {
            max_bytes: capability.max_bytes,
        };
    }

    let (delivery, family_limit) = match (family, kind) {
        (
            AdapterFamily::Anthropic | AdapterFamily::AnthropicCompatible,
            CapabilityMediaKind::Image,
        ) => (AttachmentGate::AttachToolResult, ANTHROPIC_IMAGE_BYTES),
        (
            AdapterFamily::Anthropic | AdapterFamily::AnthropicCompatible,
            CapabilityMediaKind::Pdf,
        ) => (AttachmentGate::AttachToolResult, MAX_ENCODED_BYTES as u64),
        (AdapterFamily::AwsBedrockConverse, CapabilityMediaKind::Image) => {
            (AttachmentGate::AttachToolResult, BEDROCK_IMAGE_BYTES)
        }
        (AdapterFamily::AwsBedrockConverse, CapabilityMediaKind::Pdf) => {
            (AttachmentGate::AttachToolResult, BEDROCK_PDF_BYTES)
        }
        (AdapterFamily::AwsBedrockConverse, CapabilityMediaKind::Video) => {
            (AttachmentGate::AttachToolResult, BEDROCK_VIDEO_BYTES)
        }
        (
            AdapterFamily::OpenaiResponses | AdapterFamily::AzureOpenaiResponses,
            CapabilityMediaKind::Image | CapabilityMediaKind::Pdf,
        ) => (AttachmentGate::AttachToolResult, MAX_ENCODED_BYTES as u64),
        // Families that cannot carry the kind in tool results fall back to a follow-up
        // user message whenever their adapter accepts that kind in user turns.
        (AdapterFamily::GoogleGemini, _) => {
            (AttachmentGate::DeliverViaUserTurn, GEMINI_INLINE_BYTES)
        }
        (AdapterFamily::GoogleVertexGemini, CapabilityMediaKind::Image) => {
            (AttachmentGate::DeliverViaUserTurn, VERTEX_IMAGE_BYTES)
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
            (AttachmentGate::DeliverViaUserTurn, MAX_ENCODED_BYTES as u64)
        }
        (
            AdapterFamily::OpenaiCompatible
            | AdapterFamily::AnthropicCompatible
            | AdapterFamily::GoogleVertexGemini,
            CapabilityMediaKind::Video,
        ) => (
            AttachmentGate::DeliverViaUserTurn,
            MAX_VIDEO_ENCODED_BYTES as u64,
        ),
        _ => return AttachmentGate::RejectUnsupportedFamily,
    };
    let max_bytes = capability.max_bytes.min(family_limit);
    if size > max_bytes {
        AttachmentGate::RejectTooLarge { max_bytes }
    } else {
        delivery
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
    let candidate = if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some(("image/png", MediaKind::Image(ImageContainer::Png)))
    } else if bytes.starts_with(&[0xff, 0xd8]) {
        Some(("image/jpeg", MediaKind::Image(ImageContainer::Jpeg)))
    } else if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        Some(("image/gif", MediaKind::Image(ImageContainer::Gif)))
    } else if bytes.len() >= 12 && bytes.starts_with(b"RIFF") && &bytes[8..12] == b"WEBP" {
        Some(("image/webp", MediaKind::Image(ImageContainer::WebP)))
    } else if bytes.starts_with(b"%PDF-") {
        Some(("application/pdf", MediaKind::Pdf))
    } else if bytes.starts_with(b"ID3")
        || (bytes.len() >= 2 && bytes[0] == 0xff && bytes[1] & 0xe0 == 0xe0)
    {
        // MPEG frame sync: first 11 bits set covers CRC-protected and MPEG-2/2.5 headers.
        Some(("audio/mpeg", MediaKind::Audio))
    } else if bytes.len() >= 12 && bytes.starts_with(b"RIFF") && &bytes[8..12] == b"WAVE" {
        Some(("audio/wav", MediaKind::Audio))
    } else if bytes.starts_with(b"OggS") {
        Some(("audio/ogg", MediaKind::Audio))
    } else if bytes.starts_with(b"fLaC") {
        Some(("audio/flac", MediaKind::Audio))
    } else if let Some(mime) = iso_base_media_type(bytes, len) {
        Some((mime, MediaKind::Video))
    } else if bytes.starts_with(&[0x1a, 0x45, 0xdf, 0xa3]) {
        let mime = if path
            .extension()
            .and_then(|extension| extension.to_str())
            .is_some_and(|extension| extension.eq_ignore_ascii_case("mkv"))
        {
            "video/x-matroska"
        } else {
            "video/webm"
        };
        Some((mime, MediaKind::Video))
    } else if bytes.len() >= 12 && bytes.starts_with(b"RIFF") && &bytes[8..12] == b"AVI " {
        Some(("video/x-msvideo", MediaKind::Video))
    } else if bytes.starts_with(b"FLV\x01") {
        Some(("video/x-flv", MediaKind::Video))
    } else if bytes.starts_with(&[0x00, 0x00, 0x01, 0xba])
        || bytes.starts_with(&[0x00, 0x00, 0x01, 0xb3])
    {
        Some(("video/mpeg", MediaKind::Video))
    } else if bytes.starts_with(&[
        0x30, 0x26, 0xb2, 0x75, 0x8e, 0x66, 0xcf, 0x11, 0xa6, 0xd9, 0x00, 0xaa, 0x00, 0x62, 0xce,
        0x6c,
    ]) {
        Some(("video/wmv", MediaKind::Video))
    } else {
        None
    };
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

pub fn canonical_video_mime_type(value: &str) -> &str {
    match value {
        "video/mov" => "video/quicktime",
        "video/avi" => "video/x-msvideo",
        "video/mpg" => "video/mpeg",
        _ => value,
    }
}

#[derive(Clone, Copy)]
enum ImageContainer {
    Png,
    Jpeg,
    WebP,
    Gif,
}

#[derive(Clone, Copy)]
enum MediaKind {
    Image(ImageContainer),
    Pdf,
    Audio,
    Video,
}

/// `bytes` may be a prefix of a file `len` bytes long.
fn iso_base_media_type(bytes: &[u8], len: u64) -> Option<&'static str> {
    if bytes.len() < 12 || &bytes[4..8] != b"ftyp" {
        return None;
    }
    let (box_size, major_offset, compatible_offset) =
        match u32::from_be_bytes(bytes[..4].try_into().ok()?) {
            1 => {
                if bytes.len() < 24 {
                    return None;
                }
                let size =
                    usize::try_from(u64::from_be_bytes(bytes[8..16].try_into().ok()?)).ok()?;
                (size, 16, 24)
            }
            size => (size as usize, 8, 16),
        };
    if box_size < compatible_offset || box_size as u64 > len {
        return None;
    }
    let brand = &bytes[major_offset..major_offset + 4];
    if let Some(mime) = iso_base_brand_media_type(brand) {
        return Some(mime);
    }
    // Scan the compatible-brand list with a hard bound: an oversized ftyp box must not turn
    // sniffing into a full scan of unbounded input.
    let scan_end = box_size
        .min(compatible_offset + MAX_BRAND_SCAN_BYTES)
        .min(bytes.len());
    if scan_end < compatible_offset {
        return None;
    }
    bytes[compatible_offset..scan_end]
        .as_chunks::<4>()
        .0
        .iter()
        .find_map(|brand| iso_base_brand_media_type(brand))
}

fn iso_base_brand_media_type(brand: &[u8]) -> Option<&'static str> {
    if brand == b"qt  " {
        return Some("video/quicktime");
    }
    if brand.starts_with(b"3g") {
        return Some("video/3gpp");
    }
    matches!(
        brand,
        b"isom"
            | b"iso2"
            | b"iso3"
            | b"iso4"
            | b"iso5"
            | b"iso6"
            | b"iso7"
            | b"iso8"
            | b"iso9"
            | b"mp41"
            | b"mp42"
            | b"avc1"
            | b"dash"
            | b"M4V "
    )
    .then_some("video/mp4")
}

/// Structural image validation. The container must parse exactly and the dimensions it declares
/// must stay within bounds; pixel data is never decoded. The dimension, pixel, and frame caps
/// bound what a provider-side decoder allocates, so they stand in for a trial decode.
fn validate_image(bytes: &[u8], container: ImageContainer) -> bool {
    let dimensions = match container {
        ImageContainer::Png => png_dimensions(bytes),
        ImageContainer::Jpeg => jpeg_dimensions(bytes),
        ImageContainer::WebP => static_webp_dimensions(bytes),
        ImageContainer::Gif => return gif_within_limits(bytes),
    };
    dimensions.is_some_and(|(width, height)| valid_dimensions(width, height))
}

fn valid_dimensions(width: u32, height: u32) -> bool {
    width != 0
        && height != 0
        && width <= MAX_IMAGE_DIMENSION
        && height <= MAX_IMAGE_DIMENSION
        && u64::from(width) * u64::from(height) <= MAX_IMAGE_PIXELS
}

/// Walks every chunk (CRC-checked) and returns the `IHDR` dimensions of a complete PNG.
fn png_dimensions(bytes: &[u8]) -> Option<(u32, u32)> {
    let mut offset = 8_usize;
    let mut header = None;
    let mut saw_idat = false;
    let mut saw_iend = false;
    while offset.checked_add(12).is_some_and(|end| end <= bytes.len()) {
        let length =
            u32::from_be_bytes(bytes[offset..offset + 4].try_into().expect("PNG length")) as usize;
        let chunk_type = &bytes[offset + 4..offset + 8];
        let data_end = offset.checked_add(8)?.checked_add(length)?;
        let chunk_end = data_end.checked_add(4)?;
        if chunk_end > bytes.len() || saw_iend {
            return None;
        }
        let data = &bytes[offset + 8..data_end];
        let expected = u32::from_be_bytes(bytes[data_end..chunk_end].try_into().expect("PNG CRC"));
        let mut crc = crc32fast::Hasher::new();
        crc.update(chunk_type);
        crc.update(data);
        if crc.finalize() != expected {
            return None;
        }
        match (header, chunk_type) {
            (None, b"IHDR") => header = Some(png_header(data)?),
            (None, _) | (Some(_), b"IHDR") => return None,
            _ => {}
        }
        saw_idat |= chunk_type == b"IDAT";
        saw_iend = chunk_type == b"IEND" && length == 0;
        offset = chunk_end;
    }
    if saw_idat && saw_iend && offset == bytes.len() {
        header
    } else {
        None
    }
}

fn png_header(data: &[u8]) -> Option<(u32, u32)> {
    let data: &[u8; 13] = data.try_into().ok()?;
    let [depth, color, compression, filter, interlace] =
        [data[8], data[9], data[10], data[11], data[12]];
    let valid_depth = match color {
        0 => matches!(depth, 1 | 2 | 4 | 8 | 16),
        3 => matches!(depth, 1 | 2 | 4 | 8),
        2 | 4 | 6 => matches!(depth, 8 | 16),
        _ => false,
    };
    (valid_depth && compression == 0 && filter == 0 && interlace <= 1).then_some((
        u32::from_be_bytes([data[0], data[1], data[2], data[3]]),
        u32::from_be_bytes([data[4], data[5], data[6], data[7]]),
    ))
}

/// Walks the marker segments of a complete JPEG and returns the frame header dimensions.
fn jpeg_dimensions(bytes: &[u8]) -> Option<(u32, u32)> {
    let mut offset = 2_usize;
    let mut in_scan = false;
    let mut dimensions = None;
    while offset < bytes.len() {
        if in_scan {
            offset += bytes[offset..].iter().position(|byte| *byte == 0xff)?;
        } else if bytes.get(offset) != Some(&0xff) {
            return None;
        }
        while bytes.get(offset) == Some(&0xff) {
            offset += 1;
        }
        let marker = *bytes.get(offset)?;
        offset += 1;
        match marker {
            0x00 if in_scan => {}
            0xd0..=0xd7 if in_scan => {}
            0xd9 => return dimensions.filter(|_| offset == bytes.len()),
            0x01 if !in_scan => {}
            0xd8 | 0x00 | 0xd0..=0xd7 => return None,
            _ => {
                let length_bytes = bytes.get(offset..offset + 2)?;
                let length =
                    u16::from_be_bytes(length_bytes.try_into().expect("JPEG segment")) as usize;
                if length < 2 {
                    return None;
                }
                let end = offset
                    .checked_add(length)
                    .filter(|end| *end <= bytes.len())?;
                // SOF0-SOF15, excluding DHT (C4), JPG (C8), and DAC (CC).
                if matches!(marker, 0xc0..=0xcf) && !matches!(marker, 0xc4 | 0xc8 | 0xcc) {
                    if dimensions.is_some() {
                        return None;
                    }
                    dimensions = Some(jpeg_frame_dimensions(&bytes[offset + 2..end])?);
                }
                if marker == 0xda && dimensions.is_none() {
                    return None;
                }
                offset = end;
                in_scan = marker == 0xda;
            }
        }
    }
    None
}

fn jpeg_frame_dimensions(segment: &[u8]) -> Option<(u32, u32)> {
    let &[_precision, h0, h1, w0, w1, components, ref specs @ ..] = segment else {
        return None;
    };
    ((1..=4).contains(&components) && specs.len() == usize::from(components) * 3).then_some((
        u32::from(u16::from_be_bytes([w0, w1])),
        u32::from(u16::from_be_bytes([h0, h1])),
    ))
}

/// Walks the RIFF chunks of a single-image WebP and returns its canvas dimensions.
fn static_webp_dimensions(bytes: &[u8]) -> Option<(u32, u32)> {
    if bytes.len() < 20
        || u32::from_le_bytes(bytes[4..8].try_into().expect("WebP length")) as usize + 8
            != bytes.len()
    {
        return None;
    }
    let mut offset = 12_usize;
    let mut canvas = None;
    let mut primary = None;
    let mut primary_images = 0_usize;
    while offset.checked_add(8).is_some_and(|end| end <= bytes.len()) {
        let kind = &bytes[offset..offset + 4];
        let length = u32::from_le_bytes(
            bytes[offset + 4..offset + 8]
                .try_into()
                .expect("WebP chunk length"),
        ) as usize;
        let data_end = offset.checked_add(8)?.checked_add(length)?;
        let chunk_end = data_end.checked_add(length & 1)?;
        if chunk_end > bytes.len() {
            return None;
        }
        let data = &bytes[offset + 8..data_end];
        match kind {
            b"VP8X" => {
                if length != 10 || data[0] & 0x02 != 0 {
                    return None;
                }
                let u24 = |at: usize| u32::from_le_bytes([data[at], data[at + 1], data[at + 2], 0]);
                canvas = Some((u24(4) + 1, u24(7) + 1));
            }
            b"VP8 " => {
                primary_images += 1;
                // Frame tag (key frame flag clear), start code, then 14-bit width and height.
                let &[tag, _, _, 0x9d, 0x01, 0x2a, w0, w1, h0, h1, ..] = data else {
                    return None;
                };
                if tag & 0x01 != 0 {
                    return None;
                }
                primary = Some((
                    u32::from(u16::from_le_bytes([w0, w1]) & 0x3fff),
                    u32::from(u16::from_le_bytes([h0, h1]) & 0x3fff),
                ));
            }
            b"VP8L" => {
                primary_images += 1;
                // Signature, then 14-bit width-1, 14-bit height-1, alpha hint, 3-bit version 0.
                let &[0x2f, b0, b1, b2, b3, ..] = data else {
                    return None;
                };
                let bits = u32::from_le_bytes([b0, b1, b2, b3]);
                if bits >> 29 != 0 {
                    return None;
                }
                primary = Some(((bits & 0x3fff) + 1, ((bits >> 14) & 0x3fff) + 1));
            }
            b"ANMF" => return None,
            _ => {}
        }
        offset = chunk_end;
    }
    if offset == bytes.len() && primary_images == 1 {
        canvas.or(primary)
    } else {
        None
    }
}

/// Checks a complete GIF against the dimension and frame caps. A decoder composites every frame
/// onto the full logical screen, so the pixel budget covers screen area times frame count.
fn gif_within_limits(bytes: &[u8]) -> bool {
    let Some(frames) = gif_frame_count(bytes) else {
        return false;
    };
    let width = u32::from(u16::from_le_bytes([bytes[6], bytes[7]]));
    let height = u32::from(u16::from_le_bytes([bytes[8], bytes[9]]));
    valid_dimensions(width, height)
        && frames <= MAX_ANIMATION_FRAMES
        && (u64::from(width) * u64::from(height))
            .checked_mul(frames as u64)
            .is_some_and(|pixels| pixels <= MAX_IMAGE_PIXELS)
}

/// Walks the blocks of a complete GIF and returns its image (frame) count.
fn gif_frame_count(bytes: &[u8]) -> Option<usize> {
    if bytes.len() < 14 {
        return None;
    }
    let mut offset = 13_usize;
    if bytes[10] & 0x80 != 0 {
        offset = offset
            .checked_add(3_usize << ((bytes[10] & 0x07) + 1))
            .filter(|end| *end <= bytes.len())?;
    }
    let mut images = 0_usize;
    while let Some(block) = bytes.get(offset).copied() {
        match block {
            0x2c => {
                if offset.checked_add(10).is_none_or(|end| end > bytes.len()) {
                    return None;
                }
                let packed = bytes[offset + 9];
                offset += 10;
                if packed & 0x80 != 0 {
                    offset = offset
                        .checked_add(3_usize << ((packed & 0x07) + 1))
                        .filter(|end| *end <= bytes.len())?;
                }
                if !bytes
                    .get(offset)
                    .is_some_and(|code| (2..=12).contains(code))
                {
                    return None;
                }
                offset = gif_sub_blocks(bytes, offset + 1)?;
                images += 1;
                if images > MAX_ANIMATION_FRAMES {
                    return None;
                }
            }
            0x21 => {
                if offset.checked_add(2).is_none_or(|end| end > bytes.len()) {
                    return None;
                }
                offset = gif_sub_blocks(bytes, offset + 2)?;
            }
            0x3b => return (images != 0 && offset + 1 == bytes.len()).then_some(images),
            _ => return None,
        }
    }
    None
}

fn gif_sub_blocks(bytes: &[u8], mut offset: usize) -> Option<usize> {
    loop {
        let length = *bytes.get(offset)? as usize;
        offset = offset.checked_add(1)?;
        if length == 0 {
            return Some(offset);
        }
        offset = offset.checked_add(length)?;
        if offset > bytes.len() {
            return None;
        }
    }
}

/// Structural PDF validation without parsing the object graph or decompressing streams: a
/// version header, an `%%EOF` marker near the end, and a final `startxref` whose offset lands
/// on a classic `xref` table or a cross-reference stream object. Encrypted documents are
/// rejected because providers refuse them.
fn validate_pdf(bytes: &[u8]) -> bool {
    if !(bytes.starts_with(b"%PDF-1.") || bytes.starts_with(b"%PDF-2.")) {
        return false;
    }
    let tail_start = bytes.len().saturating_sub(PDF_TAIL_BYTES);
    let Some(eof) = rfind(&bytes[tail_start..], b"%%EOF").map(|index| tail_start + index) else {
        return false;
    };
    let search_start = eof.saturating_sub(PDF_TAIL_BYTES);
    let Some(marker) = rfind(&bytes[search_start..eof], b"startxref").map(|at| search_start + at)
    else {
        return false;
    };
    if !bytes[marker - 1].is_ascii_whitespace() {
        return false;
    }
    let pointer = bytes[marker + b"startxref".len()..eof].trim_ascii();
    if pointer.is_empty() || !pointer.iter().all(u8::is_ascii_digit) {
        return false;
    }
    let Some(xref_offset) = std::str::from_utf8(pointer)
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .filter(|offset| *offset < marker)
    else {
        return false;
    };
    let xref = bytes[xref_offset..marker].trim_ascii_start();
    if !(xref.starts_with(b"xref") || pdf_object_header(xref)) {
        return false;
    }
    // The trailer (classic) or stream dictionary (xref stream) ends before the first `stream`
    // or `startxref` keyword; only that region is searched for an encryption dictionary.
    let dictionary_end = [&b"stream"[..], b"startxref"]
        .into_iter()
        .filter_map(|keyword| find(xref, keyword))
        .min()
        .unwrap_or(xref.len());
    find(&xref[..dictionary_end], b"/Encrypt").is_none()
}

/// Whether `bytes` starts with an indirect object header, `<id> <generation> obj`.
fn pdf_object_header(bytes: &[u8]) -> bool {
    let mut rest = bytes;
    for _ in 0..2 {
        let digits = rest.iter().take_while(|byte| byte.is_ascii_digit()).count();
        let spaces = rest[digits..]
            .iter()
            .take_while(|byte| byte.is_ascii_whitespace())
            .count();
        if digits == 0 || spaces == 0 {
            return false;
        }
        rest = &rest[digits + spaces..];
    }
    rest.starts_with(b"obj")
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

fn rfind(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .rposition(|window| window == needle)
}

fn known_media_extension(path: &Path) -> bool {
    path.extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| {
            matches!(
                extension.to_ascii_lowercase().as_str(),
                "png"
                    | "jpg"
                    | "jpeg"
                    | "gif"
                    | "webp"
                    | "pdf"
                    | "mp3"
                    | "wav"
                    | "ogg"
                    | "flac"
                    | "mp4"
                    | "mov"
                    | "mkv"
                    | "webm"
                    | "avi"
                    | "flv"
                    | "mpg"
                    | "mpeg"
                    | "wmv"
                    | "3gp"
            )
        })
}

fn media_extension_matches(path: &Path, mime: &str) -> bool {
    path.extension()
        .and_then(|extension| extension.to_str())
        .is_none_or(|extension| {
            matches!(
                (extension.to_ascii_lowercase().as_str(), mime),
                ("png", "image/png")
                    | ("jpg" | "jpeg", "image/jpeg")
                    | ("gif", "image/gif")
                    | ("webp", "image/webp")
                    | ("pdf", "application/pdf")
                    | ("mp3", "audio/mpeg")
                    | ("wav", "audio/wav")
                    | ("ogg", "audio/ogg")
                    | ("flac", "audio/flac")
                    | ("mp4", "video/mp4")
                    | ("mov", "video/quicktime" | "video/mov")
                    | ("mkv", "video/x-matroska")
                    | ("webm", "video/webm")
                    | ("avi", "video/x-msvideo" | "video/avi")
                    | ("flv", "video/x-flv")
                    | ("mpg" | "mpeg", "video/mpeg" | "video/mpg")
                    | ("wmv", "video/wmv")
                    | ("3gp", "video/3gpp")
            ) || !known_media_extension(path)
        })
}

fn malformed_media() -> ToolError {
    ToolError::execution("file extension identifies malformed image, PDF, audio, or video content")
}

/// Upper bound on a retained image or PDF attachment.
pub const MAX_ATTACHMENT_BYTES: u64 = 20 * 1024 * 1024;
/// Upper bound on a retained video attachment.
pub const MAX_VIDEO_ATTACHMENT_BYTES: u64 = 25 * 1024 * 1024;

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
        ModelCapabilities, ReplayCapability,
    };

    use super::{
        AttachmentGate, BEDROCK_IMAGE_BYTES, BEDROCK_PDF_BYTES, MAX_VIDEO_ATTACHMENT_BYTES,
        approved_media_type, attachment_gate_error, gate_attachment, validate_attachment,
    };
    use crate::AdapterFamily;

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
        let reject = AttachmentGate::RejectUnsupportedFamily;
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
                    AdapterFamily::OpenaiChat,
                    &capabilities(Some((MediaKind::Video, advertised, 1024))),
                    observed,
                    5,
                ),
                AttachmentGate::RejectUnsupportedFamily
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
            AttachmentGate::RejectUnsupportedFamily
        );
        assert_eq!(
            gate_attachment(AdapterFamily::Unknown, &video, "video/mp4", 5),
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
