use std::{
    collections::{BTreeMap, HashSet},
    io::Cursor,
    path::Path,
};

use image::{AnimationDecoder, ImageDecoder, ImageFormat, ImageReader, Limits};
use lopdf::{Document, LoadOptions, Object};

use crate::ToolError;
use cookie_agent_models::adapters::OvenAdapterFamily;
use cookie_agent_protocol::{
    AdaptorId, MediaKind as CapabilityMediaKind, ModelCapabilities, ModelKey,
};

const MAX_ENCODED_BYTES: usize = 20 * 1024 * 1024;
const MAX_VIDEO_ENCODED_BYTES: usize = 25 * 1024 * 1024;
const MAX_IMAGE_DIMENSION: u32 = 16_384;
const MAX_IMAGE_PIXELS: u64 = 16 * 1024 * 1024;
const MAX_DECODED_BYTES: u64 = 64 * 1024 * 1024;
const MAX_ANIMATION_FRAMES: usize = 256;
const MAX_PDF_OBJECTS: usize = 4_096;
const MAX_PDF_XREF_ENTRIES: usize = 8_192;
const MAX_PDF_OBJECT_ID: u32 = 1_000_000;
const MAX_PDF_PAGES: usize = 1_024;
const MAX_PDF_DECOMPRESSED_BYTES: usize = 16 * 1024 * 1024;
const MAX_PDF_DECOMPRESSION_RATIO: usize = 128;
const MAX_PDF_DECOMPRESSION_SLACK: usize = 64 * 1024;
const MAX_PDF_TRAILER_DEPTH: usize = 64;
const MAX_PDF_TRAILER_TOKENS: usize = 16_384;
const MAX_PDF_NAME_BYTES: usize = 256;
const MAX_PDF_STRING_BYTES: usize = 8 * 1024 * 1024;
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
    family: AdaptorId,
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
            "Cannot attach {mime_type}: not deliverable in tool results via the {} family API",
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
    family: OvenAdapterFamily,
    capabilities: &ModelCapabilities,
    mime_type: &str,
    bytes: &[u8],
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
    if bytes.len() as u64 > capability.max_bytes {
        return AttachmentGate::RejectTooLarge {
            max_bytes: capability.max_bytes,
        };
    }

    let (delivery, family_limit) = match (family, kind) {
        (
            OvenAdapterFamily::Anthropic | OvenAdapterFamily::AnthropicCompatible,
            CapabilityMediaKind::Image | CapabilityMediaKind::Pdf,
        ) => (AttachmentGate::AttachToolResult, MAX_ENCODED_BYTES as u64),
        (OvenAdapterFamily::AwsBedrockConverse, CapabilityMediaKind::Image) => {
            (AttachmentGate::AttachToolResult, BEDROCK_IMAGE_BYTES)
        }
        (OvenAdapterFamily::AwsBedrockConverse, CapabilityMediaKind::Pdf) => {
            (AttachmentGate::AttachToolResult, BEDROCK_PDF_BYTES)
        }
        (OvenAdapterFamily::AwsBedrockConverse, CapabilityMediaKind::Video) => {
            (AttachmentGate::AttachToolResult, BEDROCK_VIDEO_BYTES)
        }
        (
            OvenAdapterFamily::OpenaiResponses | OvenAdapterFamily::AzureOpenaiResponses,
            CapabilityMediaKind::Image,
        ) => (AttachmentGate::AttachToolResult, MAX_ENCODED_BYTES as u64),
        (
            OvenAdapterFamily::OpenaiCompatible
            | OvenAdapterFamily::AnthropicCompatible
            | OvenAdapterFamily::GoogleGemini
            | OvenAdapterFamily::GoogleVertexGemini,
            CapabilityMediaKind::Video,
        ) => (
            AttachmentGate::DeliverViaUserTurn,
            MAX_VIDEO_ENCODED_BYTES as u64,
        ),
        (
            OvenAdapterFamily::GoogleGemini | OvenAdapterFamily::GoogleVertexGemini,
            CapabilityMediaKind::Audio,
        ) => (AttachmentGate::DeliverViaUserTurn, MAX_ENCODED_BYTES as u64),
        _ => return AttachmentGate::RejectUnsupportedFamily,
    };
    let max_bytes = capability.max_bytes.min(family_limit);
    if bytes.len() as u64 > max_bytes {
        AttachmentGate::RejectTooLarge { max_bytes }
    } else {
        delivery
    }
}

pub(crate) fn validate_media_part_counts(
    history: &[oven_sdk::HistoryTurn],
    capabilities: &ModelCapabilities,
) -> Result<(), String> {
    let mut counts = BTreeMap::new();
    for turn in history {
        match turn {
            oven_sdk::HistoryTurn::System(_) => {}
            oven_sdk::HistoryTurn::User(message) => {
                for part in &message.content {
                    if let oven_sdk::InputPart::File(file) = part {
                        count_media_file(&mut counts, file);
                    }
                }
            }
            oven_sdk::HistoryTurn::Assistant(turn) => {
                for part in &turn.message.content {
                    match part {
                        oven_sdk::AssistantPart::File(file) => {
                            count_media_file(&mut counts, file);
                        }
                        oven_sdk::AssistantPart::ToolResult(result) => {
                            count_tool_media(&mut counts, &result.content);
                        }
                        _ => {}
                    }
                }
            }
            oven_sdk::HistoryTurn::Tool(message) => {
                for result in &message.results {
                    count_tool_media(&mut counts, &result.content);
                }
            }
        }
    }
    for (kind, capability) in &capabilities.media {
        let count = counts.get(kind).copied().unwrap_or_default();
        if count > capability.max_count as usize {
            let label = match kind {
                CapabilityMediaKind::Image => "image",
                CapabilityMediaKind::Audio => "audio",
                CapabilityMediaKind::Pdf => "PDF",
                CapabilityMediaKind::Video => "video",
            };
            return Err(format!(
                "model request contains {count} {label} file parts; the model limit is {}",
                capability.max_count
            ));
        }
    }
    Ok(())
}

fn count_media_file(counts: &mut BTreeMap<CapabilityMediaKind, usize>, file: &oven_sdk::FilePart) {
    if let Some(kind) = capability_media_kind(&file.media_type) {
        *counts.entry(kind).or_insert(0) += 1;
    }
}

fn count_tool_media(
    counts: &mut BTreeMap<CapabilityMediaKind, usize>,
    content: &oven_sdk::ToolContent,
) {
    if let oven_sdk::ToolContent::Mixed(values) = content {
        for value in values {
            if let oven_sdk::ContentValue::File(file) = value {
                count_media_file(counts, file);
            }
        }
    }
}

fn capability_media_kind(mime_type: &str) -> Option<CapabilityMediaKind> {
    if mime_type.starts_with("image/") {
        Some(CapabilityMediaKind::Image)
    } else if mime_type == "application/pdf" {
        Some(CapabilityMediaKind::Pdf)
    } else if mime_type.starts_with("audio/") {
        Some(CapabilityMediaKind::Audio)
    } else if mime_type.starts_with("video/") {
        Some(CapabilityMediaKind::Video)
    } else {
        None
    }
}

pub fn approved_media_type(path: &Path, bytes: &[u8]) -> Result<Option<&'static str>, ToolError> {
    let known_extension = known_media_extension(path);
    let candidate = if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some(("image/png", MediaKind::Image(ImageFormat::Png)))
    } else if bytes.starts_with(&[0xff, 0xd8]) {
        Some(("image/jpeg", MediaKind::Image(ImageFormat::Jpeg)))
    } else if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        Some(("image/gif", MediaKind::Gif))
    } else if bytes.len() >= 12 && bytes.starts_with(b"RIFF") && &bytes[8..12] == b"WEBP" {
        Some(("image/webp", MediaKind::Image(ImageFormat::WebP)))
    } else if bytes.starts_with(b"%PDF-") {
        Some(("application/pdf", MediaKind::Pdf))
    } else if bytes.starts_with(b"ID3")
        || bytes.starts_with(&[0xff, 0xfb])
        || bytes.starts_with(&[0xff, 0xf3])
    {
        Some(("audio/mpeg", MediaKind::Audio))
    } else if bytes.len() >= 12 && bytes.starts_with(b"RIFF") && &bytes[8..12] == b"WAVE" {
        Some(("audio/wav", MediaKind::Audio))
    } else if bytes.starts_with(b"OggS") {
        Some(("audio/ogg", MediaKind::Audio))
    } else if bytes.starts_with(b"fLaC") {
        Some(("audio/flac", MediaKind::Audio))
    } else if let Some(mime) = iso_base_media_type(bytes) {
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
        MediaKind::Image(_) | MediaKind::Gif | MediaKind::Pdf | MediaKind::Audio => {
            MAX_ENCODED_BYTES
        }
    };
    if bytes.len() > max_encoded_bytes {
        return Err(ToolError::resource_limit(format!(
            "attachment is {} bytes; the absolute validation cap is {max_encoded_bytes} bytes",
            bytes.len()
        )));
    }
    let valid = match kind {
        MediaKind::Image(format) => validate_image(bytes, format),
        MediaKind::Gif => validate_gif(bytes),
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

pub(crate) fn canonical_video_mime_type(value: &str) -> &str {
    match value {
        "video/mov" => "video/quicktime",
        "video/avi" => "video/x-msvideo",
        "video/mpg" => "video/mpeg",
        _ => value,
    }
}

#[derive(Clone, Copy)]
enum MediaKind {
    Image(ImageFormat),
    Gif,
    Pdf,
    Audio,
    Video,
}

fn iso_base_media_type(bytes: &[u8]) -> Option<&'static str> {
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
    if box_size < compatible_offset || box_size > bytes.len() {
        return None;
    }
    let brand = &bytes[major_offset..major_offset + 4];
    if let Some(mime) = iso_base_brand_media_type(brand) {
        return Some(mime);
    }
    bytes[compatible_offset..box_size]
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

fn validate_image(bytes: &[u8], format: ImageFormat) -> bool {
    let exact_container = match format {
        ImageFormat::Png => exact_png(bytes),
        ImageFormat::Jpeg => exact_jpeg(bytes),
        ImageFormat::WebP => exact_static_webp(bytes),
        _ => false,
    };
    if !exact_container {
        return false;
    }
    let reader = ImageReader::with_format(Cursor::new(bytes), format);
    let Ok((width, height)) = reader.into_dimensions() else {
        return false;
    };
    if !valid_dimensions(width, height) {
        return false;
    }
    let mut reader = ImageReader::with_format(Cursor::new(bytes), format);
    reader.limits(image_limits());
    reader.decode().is_ok()
}

fn validate_gif(bytes: &[u8]) -> bool {
    if !exact_gif(bytes) {
        return false;
    }
    let Ok(mut decoder) = image::codecs::gif::GifDecoder::new(Cursor::new(bytes)) else {
        return false;
    };
    if decoder.set_limits(image_limits()).is_err() {
        return false;
    }
    let (width, height) = decoder.dimensions();
    if !valid_dimensions(width, height) {
        return false;
    }
    let mut frames = 0_usize;
    let mut pixels = 0_u64;
    for frame in decoder.into_frames() {
        let Ok(frame) = frame else {
            return false;
        };
        frames += 1;
        if frames > MAX_ANIMATION_FRAMES {
            return false;
        }
        let buffer = frame.buffer();
        pixels = match pixels.checked_add(u64::from(buffer.width()) * u64::from(buffer.height())) {
            Some(total) if total <= MAX_IMAGE_PIXELS => total,
            _ => return false,
        };
    }
    frames != 0
}

fn image_limits() -> Limits {
    let mut limits = Limits::default();
    limits.max_image_width = Some(MAX_IMAGE_DIMENSION);
    limits.max_image_height = Some(MAX_IMAGE_DIMENSION);
    limits.max_alloc = Some(MAX_DECODED_BYTES);
    limits
}

fn valid_dimensions(width: u32, height: u32) -> bool {
    width != 0
        && height != 0
        && width <= MAX_IMAGE_DIMENSION
        && height <= MAX_IMAGE_DIMENSION
        && u64::from(width) * u64::from(height) <= MAX_IMAGE_PIXELS
}

fn exact_png(bytes: &[u8]) -> bool {
    let mut offset = 8_usize;
    let mut saw_iend = false;
    while offset.checked_add(12).is_some_and(|end| end <= bytes.len()) {
        let length =
            u32::from_be_bytes(bytes[offset..offset + 4].try_into().expect("PNG length")) as usize;
        let chunk_type = &bytes[offset + 4..offset + 8];
        let Some(data_end) = offset
            .checked_add(8)
            .and_then(|start| start.checked_add(length))
        else {
            return false;
        };
        let Some(chunk_end) = data_end.checked_add(4) else {
            return false;
        };
        if chunk_end > bytes.len() || saw_iend {
            return false;
        }
        let expected = u32::from_be_bytes(bytes[data_end..chunk_end].try_into().expect("PNG CRC"));
        let mut crc = crc32fast::Hasher::new();
        crc.update(chunk_type);
        crc.update(&bytes[offset + 8..data_end]);
        if crc.finalize() != expected {
            return false;
        }
        saw_iend = chunk_type == b"IEND" && length == 0;
        offset = chunk_end;
    }
    saw_iend && offset == bytes.len()
}

fn exact_jpeg(bytes: &[u8]) -> bool {
    let mut offset = 2_usize;
    let mut in_scan = false;
    while offset < bytes.len() {
        let marker = if !in_scan {
            if bytes.get(offset) != Some(&0xff) {
                return false;
            }
            while bytes.get(offset) == Some(&0xff) {
                offset += 1;
            }
            let Some(marker) = bytes.get(offset).copied() else {
                return false;
            };
            offset += 1;
            marker
        } else {
            let Some(relative) = bytes[offset..].iter().position(|byte| *byte == 0xff) else {
                return false;
            };
            offset += relative + 1;
            while bytes.get(offset) == Some(&0xff) {
                offset += 1;
            }
            let Some(marker) = bytes.get(offset).copied() else {
                return false;
            };
            offset += 1;
            marker
        };
        match marker {
            0x00 if in_scan => {}
            0xd0..=0xd7 if in_scan => {}
            0xd9 => return offset == bytes.len(),
            0x01 if !in_scan => {}
            0xd8 | 0x00 | 0xd0..=0xd7 => return false,
            _ => {
                let Some(length_bytes) = bytes.get(offset..offset + 2) else {
                    return false;
                };
                let length =
                    u16::from_be_bytes(length_bytes.try_into().expect("JPEG segment")) as usize;
                if length < 2 {
                    return false;
                }
                offset = match offset.checked_add(length) {
                    Some(end) if end <= bytes.len() => end,
                    _ => return false,
                };
                in_scan = marker == 0xda;
            }
        }
    }
    false
}

fn exact_static_webp(bytes: &[u8]) -> bool {
    if bytes.len() < 20
        || u32::from_le_bytes(bytes[4..8].try_into().expect("WebP length")) as usize + 8
            != bytes.len()
    {
        return false;
    }
    let mut offset = 12_usize;
    let mut primary_images = 0_usize;
    while offset.checked_add(8).is_some_and(|end| end <= bytes.len()) {
        let kind = &bytes[offset..offset + 4];
        let length = u32::from_le_bytes(
            bytes[offset + 4..offset + 8]
                .try_into()
                .expect("WebP chunk length"),
        ) as usize;
        let Some(data_end) = offset
            .checked_add(8)
            .and_then(|start| start.checked_add(length))
        else {
            return false;
        };
        let Some(chunk_end) = data_end.checked_add(length & 1) else {
            return false;
        };
        if chunk_end > bytes.len() {
            return false;
        }
        if kind == b"VP8X" {
            if length != 10 || bytes[offset + 8] & 0x02 != 0 {
                return false;
            }
        } else if matches!(kind, b"VP8 " | b"VP8L") {
            primary_images += 1;
        } else if kind == b"ANMF" {
            return false;
        }
        offset = chunk_end;
    }
    offset == bytes.len() && primary_images == 1
}

fn exact_gif(bytes: &[u8]) -> bool {
    if bytes.len() < 14 {
        return false;
    }
    let mut offset = 13_usize;
    if bytes[10] & 0x80 != 0 {
        offset = match offset.checked_add(3_usize << ((bytes[10] & 0x07) + 1)) {
            Some(end) if end <= bytes.len() => end,
            _ => return false,
        };
    }
    let mut images = 0_usize;
    while let Some(block) = bytes.get(offset).copied() {
        match block {
            0x2c => {
                if offset.checked_add(10).is_none_or(|end| end > bytes.len()) {
                    return false;
                }
                let packed = bytes[offset + 9];
                offset += 10;
                if packed & 0x80 != 0 {
                    offset = match offset.checked_add(3_usize << ((packed & 0x07) + 1)) {
                        Some(end) if end <= bytes.len() => end,
                        _ => return false,
                    };
                }
                if !bytes
                    .get(offset)
                    .is_some_and(|code| (2..=12).contains(code))
                {
                    return false;
                }
                offset += 1;
                let Some(end) = gif_sub_blocks(bytes, offset) else {
                    return false;
                };
                offset = end;
                images += 1;
            }
            0x21 => {
                if offset.checked_add(2).is_none_or(|end| end > bytes.len()) {
                    return false;
                }
                let Some(end) = gif_sub_blocks(bytes, offset + 2) else {
                    return false;
                };
                offset = end;
            }
            0x3b => return images != 0 && offset + 1 == bytes.len(),
            _ => return false,
        }
    }
    false
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

fn validate_pdf(bytes: &[u8]) -> bool {
    validate_pdf_bounded(bytes).is_ok()
}

fn validate_pdf_bounded(bytes: &[u8]) -> Result<usize, usize> {
    let Some(preflight) = preflight_classic_pdf(bytes) else {
        return Err(0);
    };
    let options = LoadOptions {
        filter: Some(reject_eager_pdf_container_streams),
        strict: true,
        // The accepted contract excludes xref and object streams before load.
        // A zero eager-decode allowance is a final guard against parser drift.
        max_decompressed_size: Some(0),
        ..LoadOptions::default()
    };
    let Ok(document) = Document::load_mem_with_options(bytes, options) else {
        return Err(0);
    };
    if document.is_encrypted()
        || document.objects.is_empty()
        || document.objects.len() > MAX_PDF_OBJECTS
        || document.objects.len() != preflight.normal_objects
        || document.catalog().is_err()
        || !valid_pdf_xref_objects(bytes, &document)
    {
        return Err(0);
    }
    let pages = document.get_pages();
    if pages.is_empty() || pages.len() > MAX_PDF_PAGES {
        return Err(0);
    }
    let mut total_decompressed = 0_usize;
    let mut total_encoded_streams = 0_usize;
    for object in document.objects.values() {
        let Object::Stream(stream) = object else {
            continue;
        };
        total_encoded_streams = match total_encoded_streams.checked_add(stream.content.len()) {
            Some(total) if total <= MAX_ENCODED_BYTES => total,
            _ => return Err(total_decompressed),
        };
        let remaining = MAX_PDF_DECOMPRESSED_BYTES.saturating_sub(total_decompressed);
        if stream.dict.get(b"Filter").is_err() {
            total_decompressed = match total_decompressed.checked_add(stream.content.len()) {
                Some(total) if total <= MAX_PDF_DECOMPRESSED_BYTES => total,
                _ => return Err(total_decompressed),
            };
            continue;
        }
        if remaining == 0 || stream.filters().is_err() || stream.content.is_empty() {
            return Err(total_decompressed);
        }
        let ratio_limit = stream
            .content
            .len()
            .checked_mul(MAX_PDF_DECOMPRESSION_RATIO)
            .and_then(|limit| limit.checked_add(MAX_PDF_DECOMPRESSION_SLACK))
            .unwrap_or(usize::MAX);
        let decode_limit = remaining.min(ratio_limit);
        if decode_limit == 0 {
            return Err(total_decompressed);
        }
        let Ok(content) = stream.decompressed_content_with_limit(decode_limit) else {
            return Err(total_decompressed.saturating_add(decode_limit));
        };
        total_decompressed = match total_decompressed.checked_add(content.len()) {
            Some(total) if total <= MAX_PDF_DECOMPRESSED_BYTES => total,
            _ => return Err(total_decompressed.saturating_add(decode_limit)),
        };
    }
    Ok(total_decompressed)
}

fn reject_eager_pdf_container_streams(
    object_id: (u32, u16),
    object: &mut Object,
) -> Option<((u32, u16), Object)> {
    if object.as_stream().is_ok_and(|stream| {
        stream
            .dict
            .get_type()
            .is_ok_and(|kind| matches!(kind, b"ObjStm" | b"XRef"))
    }) {
        return None;
    }
    Some((object_id, object.clone()))
}

#[derive(Clone, Copy)]
struct PdfPreflight {
    normal_objects: usize,
}

fn preflight_classic_pdf(bytes: &[u8]) -> Option<PdfPreflight> {
    let (xref_offset, startxref_marker) = pdf_startxref(bytes)?;
    let mut cursor = xref_offset;
    let xref_line = read_pdf_line(bytes, &mut cursor)?;
    if xref_line.trim_ascii() != b"xref" {
        // Cross-reference streams necessarily decompress before lopdf exposes
        // the document, so they are outside this bounded validation contract.
        return None;
    }

    let mut ids = HashSet::new();
    let mut total_entries = 0_usize;
    let mut normal_objects = 0_usize;
    let mut maximum_id = 0_u32;
    loop {
        skip_pdf_whitespace(bytes, &mut cursor);
        if consume_pdf_keyword(bytes, &mut cursor, b"trailer") {
            break;
        }
        let line = read_pdf_line(bytes, &mut cursor)?.trim_ascii();
        if line.is_empty() {
            continue;
        }
        let (start, count) = parse_xref_subsection(line)?;
        let count = usize::try_from(count).ok()?;
        total_entries = total_entries.checked_add(count)?;
        if total_entries > MAX_PDF_XREF_ENTRIES {
            return None;
        }
        let end_id = start.checked_add(u32::try_from(count).ok()?)?;
        if end_id > MAX_PDF_OBJECT_ID {
            return None;
        }
        maximum_id = maximum_id.max(end_id.saturating_sub(1));
        for index in 0..count {
            let id = start.checked_add(u32::try_from(index).ok()?)?;
            if !ids.insert(id) {
                return None;
            }
            let entry = parse_xref_entry(read_pdf_line(bytes, &mut cursor)?.trim_ascii())?;
            if entry.normal {
                if entry.offset >= xref_offset || id == 0 {
                    return None;
                }
                normal_objects += 1;
                if normal_objects > MAX_PDF_OBJECTS {
                    return None;
                }
            }
        }
    }
    if normal_objects == 0 || cursor >= startxref_marker {
        return None;
    }
    let trailer = parse_pdf_trailer(&bytes[cursor..startxref_marker])?;
    if trailer.has_prev
        || trailer.has_xref_stream
        || trailer.size == 0
        || trailer.size > u64::from(MAX_PDF_OBJECT_ID)
        || trailer.size <= u64::from(maximum_id)
    {
        return None;
    }
    Some(PdfPreflight { normal_objects })
}

#[derive(Clone, Copy)]
struct XrefEntryPreflight {
    offset: usize,
    normal: bool,
}

fn parse_xref_subsection(line: &[u8]) -> Option<(u32, u32)> {
    let mut fields = line
        .split(|byte| byte.is_ascii_whitespace())
        .filter(|field| !field.is_empty());
    let start = parse_ascii_u32(fields.next()?)?;
    let count = parse_ascii_u32(fields.next()?)?;
    fields.next().is_none().then_some((start, count))
}

fn parse_xref_entry(line: &[u8]) -> Option<XrefEntryPreflight> {
    let mut fields = line
        .split(|byte| byte.is_ascii_whitespace())
        .filter(|field| !field.is_empty());
    let offset = parse_ascii_usize(fields.next()?)?;
    let generation = parse_ascii_u32(fields.next()?)?;
    let status = fields.next()?;
    if fields.next().is_some() || generation > u32::from(u16::MAX) {
        return None;
    }
    let normal = match status {
        b"n" => true,
        b"f" => false,
        _ => return None,
    };
    Some(XrefEntryPreflight { offset, normal })
}

fn parse_ascii_u32(bytes: &[u8]) -> Option<u32> {
    std::str::from_utf8(bytes).ok()?.parse().ok()
}

fn parse_ascii_usize(bytes: &[u8]) -> Option<usize> {
    std::str::from_utf8(bytes).ok()?.parse().ok()
}

fn read_pdf_line<'a>(bytes: &'a [u8], cursor: &mut usize) -> Option<&'a [u8]> {
    let start = *cursor;
    let relative_end = bytes
        .get(start..)?
        .iter()
        .position(|byte| matches!(byte, b'\r' | b'\n'));
    let end = relative_end.map_or(bytes.len(), |relative| start + relative);
    *cursor = end;
    if bytes.get(*cursor) == Some(&b'\r') {
        *cursor += 1;
    }
    if bytes.get(*cursor) == Some(&b'\n') {
        *cursor += 1;
    }
    Some(&bytes[start..end])
}

fn skip_pdf_whitespace(bytes: &[u8], cursor: &mut usize) {
    while bytes.get(*cursor).is_some_and(u8::is_ascii_whitespace) {
        *cursor += 1;
    }
}

fn consume_pdf_keyword(bytes: &[u8], cursor: &mut usize, keyword: &[u8]) -> bool {
    let Some(end) = (*cursor).checked_add(keyword.len()) else {
        return false;
    };
    if bytes.get(*cursor..end) != Some(keyword)
        || bytes
            .get(end)
            .is_some_and(|byte| !is_pdf_delimiter(*byte) && !byte.is_ascii_whitespace())
    {
        return false;
    }
    *cursor = end;
    true
}

#[derive(Default)]
struct PdfTrailerPreflight {
    size: u64,
    has_prev: bool,
    has_xref_stream: bool,
}

fn parse_pdf_trailer(bytes: &[u8]) -> Option<PdfTrailerPreflight> {
    let mut lexer = PdfLexer::new(bytes);
    if !matches!(lexer.next()?, PdfToken::DictStart) {
        return None;
    }
    let mut trailer = PdfTrailerPreflight::default();
    loop {
        match lexer.next()? {
            PdfToken::DictEnd => break,
            PdfToken::Name(name) => {
                let value = lexer.next()?;
                if name == b"Size" {
                    let PdfToken::Integer(size) = value else {
                        return None;
                    };
                    trailer.size = u64::try_from(size).ok()?;
                } else {
                    trailer.has_prev |= name == b"Prev";
                    trailer.has_xref_stream |= name == b"XRefStm";
                    skip_pdf_value(&mut lexer, value, 1)?;
                }
            }
            _ => return None,
        }
    }
    lexer.only_whitespace_and_comments().then_some(trailer)
}

#[derive(Clone)]
struct PdfLexer<'a> {
    bytes: &'a [u8],
    cursor: usize,
    tokens: usize,
}

impl<'a> PdfLexer<'a> {
    const fn new(bytes: &'a [u8]) -> Self {
        Self {
            bytes,
            cursor: 0,
            tokens: 0,
        }
    }

    fn next(&mut self) -> Option<PdfToken<'a>> {
        self.skip_space_and_comments();
        self.tokens = self.tokens.checked_add(1)?;
        if self.tokens > MAX_PDF_TRAILER_TOKENS {
            return None;
        }
        let byte = *self.bytes.get(self.cursor)?;
        match byte {
            b'<' if self.bytes.get(self.cursor + 1) == Some(&b'<') => {
                self.cursor += 2;
                Some(PdfToken::DictStart)
            }
            b'>' if self.bytes.get(self.cursor + 1) == Some(&b'>') => {
                self.cursor += 2;
                Some(PdfToken::DictEnd)
            }
            b'[' => {
                self.cursor += 1;
                Some(PdfToken::ArrayStart)
            }
            b']' => {
                self.cursor += 1;
                Some(PdfToken::ArrayEnd)
            }
            b'/' => self.read_name().map(PdfToken::Name),
            b'(' => {
                self.skip_literal_string()?;
                Some(PdfToken::Atomic)
            }
            b'<' => {
                self.skip_hex_string()?;
                Some(PdfToken::Atomic)
            }
            b'+' | b'-' | b'.' | b'0'..=b'9' => self.read_number_or_atomic(),
            _ => self.read_keyword().map(PdfToken::Keyword),
        }
    }

    fn skip_space_and_comments(&mut self) {
        loop {
            while self
                .bytes
                .get(self.cursor)
                .is_some_and(u8::is_ascii_whitespace)
            {
                self.cursor += 1;
            }
            if self.bytes.get(self.cursor) != Some(&b'%') {
                return;
            }
            while self
                .bytes
                .get(self.cursor)
                .is_some_and(|byte| !matches!(byte, b'\r' | b'\n'))
            {
                self.cursor += 1;
            }
        }
    }

    fn only_whitespace_and_comments(mut self) -> bool {
        self.skip_space_and_comments();
        self.cursor == self.bytes.len()
    }

    fn read_name(&mut self) -> Option<Vec<u8>> {
        self.cursor += 1;
        let mut name = Vec::new();
        while let Some(&byte) = self.bytes.get(self.cursor) {
            if byte.is_ascii_whitespace() || is_pdf_delimiter(byte) {
                break;
            }
            if byte == b'#' {
                let high = pdf_hex(*self.bytes.get(self.cursor + 1)?)?;
                let low = pdf_hex(*self.bytes.get(self.cursor + 2)?)?;
                name.push((high << 4) | low);
                self.cursor += 3;
            } else {
                name.push(byte);
                self.cursor += 1;
            }
            if name.len() > MAX_PDF_NAME_BYTES {
                return None;
            }
        }
        Some(name)
    }

    fn skip_literal_string(&mut self) -> Option<()> {
        let start = self.cursor;
        self.cursor += 1;
        let mut depth = 1_usize;
        while let Some(&byte) = self.bytes.get(self.cursor) {
            if self.cursor.saturating_sub(start) > MAX_PDF_STRING_BYTES {
                return None;
            }
            match byte {
                b'\\' => {
                    self.cursor += 1;
                    if self.bytes.get(self.cursor) == Some(&b'\r') {
                        self.cursor += 1;
                        if self.bytes.get(self.cursor) == Some(&b'\n') {
                            self.cursor += 1;
                        }
                    } else if self.bytes.get(self.cursor) == Some(&b'\n') {
                        self.cursor += 1;
                    } else {
                        self.cursor = self.cursor.checked_add(1)?;
                    }
                }
                b'(' => {
                    depth = depth.checked_add(1)?;
                    if depth > 100 {
                        return None;
                    }
                    self.cursor += 1;
                }
                b')' => {
                    depth -= 1;
                    self.cursor += 1;
                    if depth == 0 {
                        return Some(());
                    }
                }
                _ => self.cursor += 1,
            }
        }
        None
    }

    fn skip_hex_string(&mut self) -> Option<()> {
        let start = self.cursor;
        self.cursor += 1;
        while let Some(&byte) = self.bytes.get(self.cursor) {
            if self.cursor.saturating_sub(start) > MAX_PDF_STRING_BYTES {
                return None;
            }
            self.cursor += 1;
            if byte == b'>' {
                return Some(());
            }
            if !byte.is_ascii_whitespace() && !byte.is_ascii_hexdigit() {
                return None;
            }
        }
        None
    }

    fn read_number_or_atomic(&mut self) -> Option<PdfToken<'a>> {
        let start = self.cursor;
        while self
            .bytes
            .get(self.cursor)
            .is_some_and(|byte| matches!(byte, b'+' | b'-' | b'.' | b'0'..=b'9'))
        {
            self.cursor += 1;
        }
        let bytes = &self.bytes[start..self.cursor];
        std::str::from_utf8(bytes)
            .ok()
            .and_then(|value| value.parse::<i64>().ok())
            .map_or(Some(PdfToken::Atomic), |value| {
                Some(PdfToken::Integer(value))
            })
    }

    fn read_keyword(&mut self) -> Option<&'a [u8]> {
        let start = self.cursor;
        while self
            .bytes
            .get(self.cursor)
            .is_some_and(|byte| !byte.is_ascii_whitespace() && !is_pdf_delimiter(*byte))
        {
            self.cursor += 1;
        }
        (self.cursor > start).then_some(&self.bytes[start..self.cursor])
    }
}

enum PdfToken<'a> {
    DictStart,
    DictEnd,
    ArrayStart,
    ArrayEnd,
    Name(Vec<u8>),
    Integer(i64),
    Keyword(&'a [u8]),
    Atomic,
}

fn skip_pdf_value(lexer: &mut PdfLexer<'_>, token: PdfToken<'_>, depth: usize) -> Option<()> {
    if depth > MAX_PDF_TRAILER_DEPTH {
        return None;
    }
    match token {
        PdfToken::DictStart => loop {
            match lexer.next()? {
                PdfToken::DictEnd => break Some(()),
                PdfToken::Name(_) => {
                    let value = lexer.next()?;
                    skip_pdf_value(lexer, value, depth + 1)?;
                }
                _ => break None,
            }
        },
        PdfToken::ArrayStart => loop {
            let value = lexer.next()?;
            if matches!(value, PdfToken::ArrayEnd) {
                break Some(());
            }
            skip_pdf_value(lexer, value, depth + 1)?;
        },
        PdfToken::Integer(_) => {
            let mut reference = lexer.clone();
            if matches!(reference.next(), Some(PdfToken::Integer(_)))
                && matches!(reference.next(), Some(PdfToken::Keyword(b"R")))
            {
                *lexer = reference;
            }
            Some(())
        }
        PdfToken::DictEnd | PdfToken::ArrayEnd => None,
        PdfToken::Name(_) | PdfToken::Keyword(_) | PdfToken::Atomic => Some(()),
    }
}

fn pdf_hex(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

fn is_pdf_delimiter(byte: u8) -> bool {
    matches!(
        byte,
        b'(' | b')' | b'<' | b'>' | b'[' | b']' | b'{' | b'}' | b'/' | b'%'
    )
}

fn valid_pdf_xref_objects(bytes: &[u8], document: &Document) -> bool {
    let mut entries = document
        .reference_table
        .entries
        .iter()
        .filter_map(|(id, entry)| match entry {
            lopdf::xref::XrefEntry::Normal { offset, generation } => {
                Some((*id, *generation, *offset as usize))
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    entries.sort_by_key(|(_, _, offset)| *offset);
    if entries.is_empty() {
        return false;
    }
    let Some(startxref_marker) = bytes.windows(9).rposition(|window| window == b"startxref") else {
        return false;
    };
    for (index, (id, generation, offset)) in entries.iter().copied().enumerate() {
        let end = entries.get(index + 1).map_or_else(
            || {
                if offset == document.xref_start {
                    startxref_marker
                } else {
                    document.xref_start
                }
            },
            |(_, _, offset)| *offset,
        );
        let Some(mut object) = bytes.get(offset..end) else {
            return false;
        };
        object = trim_ascii_start(object);
        let Some((found_id, rest)) = parse_pdf_u32(object) else {
            return false;
        };
        let Some((found_generation, rest)) = parse_pdf_u32(trim_ascii_start(rest)) else {
            return false;
        };
        let rest = trim_ascii_start(rest);
        if found_id != id || found_generation != u32::from(generation) || !rest.starts_with(b"obj")
        {
            return false;
        }
        let Some(endobj) = object.windows(6).rposition(|window| window == b"endobj") else {
            return false;
        };
        if object[endobj + 6..]
            .iter()
            .any(|byte| !byte.is_ascii_whitespace())
        {
            return false;
        }
    }
    true
}

fn parse_pdf_u32(bytes: &[u8]) -> Option<(u32, &[u8])> {
    let digits = bytes
        .iter()
        .take_while(|byte| byte.is_ascii_digit())
        .count();
    if digits == 0 {
        return None;
    }
    let value = std::str::from_utf8(&bytes[..digits]).ok()?.parse().ok()?;
    Some((value, &bytes[digits..]))
}

fn trim_ascii_start(bytes: &[u8]) -> &[u8] {
    let start = bytes
        .iter()
        .position(|byte| !byte.is_ascii_whitespace())
        .unwrap_or(bytes.len());
    &bytes[start..]
}

fn pdf_startxref(bytes: &[u8]) -> Option<(usize, usize)> {
    if bytes.len() < 32 || !(bytes.starts_with(b"%PDF-1.") || bytes.starts_with(b"%PDF-2.")) {
        return None;
    }
    let mut cursor = bytes.len();
    while cursor != 0 && bytes[cursor - 1].is_ascii_whitespace() {
        cursor -= 1;
    }
    if cursor < 5 || &bytes[cursor - 5..cursor] != b"%%EOF" {
        return None;
    }
    cursor -= 5;
    while cursor != 0 && bytes[cursor - 1].is_ascii_whitespace() {
        cursor -= 1;
    }
    let digits_end = cursor;
    while cursor != 0 && bytes[cursor - 1].is_ascii_digit() {
        cursor -= 1;
    }
    if cursor == digits_end {
        return None;
    }
    let xref_offset = parse_ascii_usize(&bytes[cursor..digits_end])?;
    while cursor != 0 && bytes[cursor - 1].is_ascii_whitespace() {
        cursor -= 1;
    }
    if cursor < 9 || &bytes[cursor - 9..cursor] != b"startxref" {
        return None;
    }
    let startxref_marker = cursor - 9;
    if startxref_marker != 0 && !bytes[startxref_marker - 1].is_ascii_whitespace() {
        return None;
    }
    (xref_offset < startxref_marker).then_some((xref_offset, startxref_marker))
}

#[cfg(test)]
pub(crate) fn pdf_validation_stats(bytes: &[u8]) -> (bool, usize) {
    match validate_pdf_bounded(bytes) {
        Ok(reserved) => (true, reserved),
        Err(reserved) => (false, reserved),
    }
}

#[cfg(test)]
pub(crate) const PDF_DECOMPRESSION_BUDGET_FOR_TESTS: usize = MAX_PDF_DECOMPRESSED_BYTES;

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

#[cfg(test)]
mod tests {
    use std::{collections::BTreeMap, path::Path};

    use cookie_agent_models::adapters::OvenAdapterFamily;
    use cookie_agent_protocol::{
        AdaptorId, CancellationCapability, MediaCapability, MediaKind, MimeType, Modality,
        ModelCapabilities, ReplayCapability,
    };

    use super::{
        AttachmentGate, BEDROCK_IMAGE_BYTES, BEDROCK_PDF_BYTES, PDF_DECOMPRESSION_BUDGET_FOR_TESTS,
        approved_media_type, attachment_gate_error, gate_attachment, pdf_validation_stats,
        validate_media_part_counts,
    };

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

    #[test]
    fn malformed_pdf_is_rejected_within_the_bounded_budget() {
        let (valid, reserved) = pdf_validation_stats(b"not a PDF");
        assert!(!valid);
        assert!(reserved <= PDF_DECOMPRESSION_BUDGET_FOR_TESTS);
    }

    #[test]
    fn request_media_count_includes_tool_results_and_emitted_user_turns() {
        let file = || {
            oven_sdk::FilePart::image(
                "image/png",
                oven_sdk::FileSource::Bytes(bytes::Bytes::from_static(b"image")),
            )
        };
        let history = vec![
            oven_sdk::HistoryTurn::tool(oven_sdk::ToolMessage::new(vec![
                oven_sdk::ToolResultPart::new(
                    "call",
                    oven_sdk::ToolContent::Mixed(vec![oven_sdk::ContentValue::File(file())]),
                ),
            ])),
            oven_sdk::HistoryTurn::user(oven_sdk::UserMessage::new(vec![
                oven_sdk::InputPart::File(file()),
            ])),
        ];
        let error = validate_media_part_counts(
            &history,
            &capabilities(Some((MediaKind::Image, "image/png", u64::MAX))),
        )
        .expect_err("two image parts must exceed max_count one");

        assert!(
            error
                .to_string()
                .contains("model request contains 2 image file parts; the model limit is 1")
        );
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
        let families = [
            (OvenAdapterFamily::Anthropic, true, true),
            (OvenAdapterFamily::AnthropicCompatible, true, true),
            (OvenAdapterFamily::OpenaiChat, false, false),
            (OvenAdapterFamily::OpenaiResponses, true, false),
            (OvenAdapterFamily::OpenaiCompatible, false, false),
            (OvenAdapterFamily::GoogleGemini, false, false),
            (OvenAdapterFamily::GoogleVertexGemini, false, false),
            (OvenAdapterFamily::AwsBedrockConverse, true, true),
            (OvenAdapterFamily::AzureOpenaiChat, false, false),
            (OvenAdapterFamily::AzureOpenaiResponses, true, false),
            (OvenAdapterFamily::CohereV2Chat, false, false),
        ];
        for (family, image_deliverable, pdf_deliverable) in families {
            for (kind, mime_type, deliverable) in [
                (MediaKind::Image, "image/png", image_deliverable),
                (MediaKind::Pdf, "application/pdf", pdf_deliverable),
            ] {
                assert_eq!(
                    gate_attachment(family, &capabilities(None), mime_type, b"media"),
                    AttachmentGate::RejectUnsupportedModel,
                    "{family:?} {kind:?} without capability"
                );
                assert_eq!(
                    gate_attachment(
                        family,
                        &capabilities(Some((kind, mime_type, 20 * 1024 * 1024))),
                        mime_type,
                        b"media",
                    ),
                    if deliverable {
                        AttachmentGate::AttachToolResult
                    } else {
                        AttachmentGate::RejectUnsupportedFamily
                    },
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
                    OvenAdapterFamily::OpenaiChat,
                    &capabilities(Some((MediaKind::Video, advertised, 1024))),
                    observed,
                    b"video",
                ),
                AttachmentGate::RejectUnsupportedFamily
            );
        }
        let image = capabilities(Some((MediaKind::Image, "image/png", u64::MAX)));
        let pdf = capabilities(Some((MediaKind::Pdf, "application/pdf", u64::MAX)));
        assert_eq!(
            gate_attachment(
                OvenAdapterFamily::AwsBedrockConverse,
                &image,
                "image/png",
                &vec![0; BEDROCK_IMAGE_BYTES as usize + 1],
            ),
            AttachmentGate::RejectTooLarge {
                max_bytes: BEDROCK_IMAGE_BYTES
            }
        );
        assert_eq!(
            gate_attachment(
                OvenAdapterFamily::AwsBedrockConverse,
                &pdf,
                "application/pdf",
                &vec![0; BEDROCK_PDF_BYTES as usize + 1],
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
                AdaptorId::AwsBedrockConverse,
            )
            .unwrap(),
            "Cannot attach image/png: exceeds the 3.75 MiB inline limit for this provider"
        );
    }

    #[test]
    fn bedrock_video_attaches_within_the_raw_byte_budget() {
        let video = capabilities(Some((MediaKind::Video, "video/mp4", u64::MAX)));
        assert_eq!(
            gate_attachment(
                OvenAdapterFamily::AwsBedrockConverse,
                &video,
                "video/mp4",
                b"video",
            ),
            AttachmentGate::AttachToolResult
        );
        assert_eq!(
            gate_attachment(
                OvenAdapterFamily::AwsBedrockConverse,
                &video,
                "video/mp4",
                &vec![0; super::BEDROCK_VIDEO_BYTES as usize],
            ),
            AttachmentGate::AttachToolResult
        );
        assert_eq!(
            gate_attachment(
                OvenAdapterFamily::AwsBedrockConverse,
                &video,
                "video/mp4",
                &vec![0; super::BEDROCK_VIDEO_BYTES as usize + 1],
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
            gate_attachment(
                OvenAdapterFamily::OpenaiChat,
                &small_cap,
                "application/pdf",
                &[0; 16],
            ),
            AttachmentGate::RejectTooLarge { max_bytes: 8 }
        );
    }

    #[test]
    fn user_turn_video_families_use_the_emitted_delivery_channel() {
        let video = capabilities(Some((MediaKind::Video, "video/mp4", 1024)));
        for family in [
            OvenAdapterFamily::OpenaiCompatible,
            OvenAdapterFamily::AnthropicCompatible,
            OvenAdapterFamily::GoogleGemini,
            OvenAdapterFamily::GoogleVertexGemini,
        ] {
            assert_eq!(
                gate_attachment(family, &video, "video/mp4", b"video"),
                AttachmentGate::DeliverViaUserTurn,
                "{family:?}"
            );
        }
        assert_eq!(
            gate_attachment(
                OvenAdapterFamily::AwsBedrockConverse,
                &video,
                "video/mp4",
                b"video"
            ),
            AttachmentGate::AttachToolResult
        );
        assert_eq!(
            gate_attachment(
                OvenAdapterFamily::OpenaiCompatible,
                &capabilities(None),
                "video/mp4",
                b"video"
            ),
            AttachmentGate::RejectUnsupportedModel
        );
        assert_eq!(
            gate_attachment(OvenAdapterFamily::Anthropic, &video, "video/mp4", b"video"),
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
            OvenAdapterFamily::GoogleGemini,
            OvenAdapterFamily::GoogleVertexGemini,
        ] {
            assert_eq!(
                gate_attachment(family, &audio, "audio/mpeg", b"ID3payload"),
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
}
