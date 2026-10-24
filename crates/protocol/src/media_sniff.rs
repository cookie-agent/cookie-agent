//! Recognizing media from its content: the format a file starts like, and
//! the structural checks an attachment must pass (image dimensions, PDF
//! structure). Pure byte inspection, shared by the daemon's tools and the
//! clients, the web client in a browser included.

use std::path::Path;

use crate::MediaKindLabel;

/// Upper bound on the compatible-brand list scanned when the major brand is unrecognized.
const MAX_BRAND_SCAN_BYTES: usize = 4 * 1024;
/// The longest side, in pixels, of an image the daemon accepts.
pub const MAX_IMAGE_DIMENSION: u32 = 16_384;
/// The most pixels of an image the daemon accepts (summed over a GIF's frames).
pub const MAX_IMAGE_PIXELS: u64 = 16 * 1024 * 1024;
const MAX_ANIMATION_FRAMES: usize = 256;
/// Window at the end of a PDF searched for `%%EOF`, and before it for `startxref`; readers
/// tolerate trailing junk within about this distance.
const PDF_TAIL_BYTES: usize = 1024;

/// Upper bound on a retained image or PDF attachment.
pub const MAX_ATTACHMENT_BYTES: u64 = 20 * 1024 * 1024;
/// Upper bound on a retained video attachment.
pub const MAX_VIDEO_ATTACHMENT_BYTES: u64 = 25 * 1024 * 1024;
/// Upper bound on an uploaded text file, which an ACP agent takes whole in
/// its prompt.
pub const MAX_TEXT_ATTACHMENT_BYTES: u64 = 1024 * 1024;
/// The media type of an uploaded text file.
pub const TEXT_MEDIA_TYPE: &str = "text/plain";

/// Whether `accepted`, a media type a model lists (exact, or a prefix
/// ending in `*`), accepts `mime_type`. Case does not matter, and a video
/// type's aliases are one type.
#[must_use]
pub fn mime_type_matches(accepted: &str, mime_type: &str) -> bool {
    match accepted.strip_suffix('*') {
        Some(prefix) => mime_type
            .get(..prefix.len())
            .is_some_and(|head| head.eq_ignore_ascii_case(prefix)),
        None => {
            accepted.eq_ignore_ascii_case(mime_type)
                || canonical_video_mime_type(accepted) == canonical_video_mime_type(mime_type)
        }
    }
}

/// The media kind label of `mime_type`: `image`, `pdf`, `audio`, or `video`.
#[must_use]
pub fn media_kind(mime_type: &str) -> Option<&'static str> {
    if mime_type.starts_with("image/") {
        Some(MediaKindLabel::IMAGE)
    } else if mime_type == "application/pdf" {
        Some(MediaKindLabel::PDF)
    } else if mime_type.starts_with("audio/") {
        Some(MediaKindLabel::AUDIO)
    } else if mime_type.starts_with("video/") {
        Some(MediaKindLabel::VIDEO)
    } else {
        None
    }
}

/// The attachment cap of a media kind: [`MAX_VIDEO_ATTACHMENT_BYTES`] for
/// video, [`MAX_TEXT_ATTACHMENT_BYTES`] for text, [`MAX_ATTACHMENT_BYTES`]
/// for everything else.
#[must_use]
pub fn max_attachment_bytes(kind: &str) -> u64 {
    if kind == MediaKindLabel::VIDEO {
        MAX_VIDEO_ATTACHMENT_BYTES
    } else if kind == MediaKindLabel::TEXT {
        MAX_TEXT_ATTACHMENT_BYTES
    } else {
        MAX_ATTACHMENT_BYTES
    }
}

/// The media type and kind `bytes`, the head of a file `len` bytes long,
/// start like; `path`'s extension tells Matroska from WebM.
pub fn recognize(path: &Path, bytes: &[u8], len: u64) -> Option<(&'static str, MediaKind)> {
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
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
    }
}

/// The media type a file whose first bytes (the runtime reads
/// `MEDIA_SNIFF_BYTES`) are `head` and which is `len` bytes long starts like, recognized from its content
/// alone; nothing is validated.
#[must_use]
pub fn recognize_media_type(head: &[u8], len: u64) -> Option<&'static str> {
    recognize(Path::new(""), head, len).map(|(media_type, _)| media_type)
}

/// What a client's upload is, recognized from its content alone and
/// validated as a tool attachment is.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ValidatedUpload {
    pub media_type: &'static str,
    /// The kind label, such as `image`.
    pub kind: &'static str,
    /// An image's width and height.
    pub image: Option<(u32, u32)>,
}

/// Why an upload's content is refused.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum UploadRejection {
    /// Not a media type the daemon recognizes.
    Unrecognized,
    /// It starts like `media_type` but does not parse as one.
    Malformed { media_type: &'static str },
    /// Above the attachment cap of its kind, or an image above the
    /// dimension or pixel limits.
    TooLarge {
        kind: &'static str,
        byte_length: u64,
        max_bytes: u64,
        image: Option<(u32, u32)>,
    },
}

/// Recognizes and validates an upload: its media type comes from its content
/// alone, never from a filename, and it is held to the checks and caps a
/// tool attachment is (the runtime's `validate_attachment`), with an
/// image's size reported. Content that is no media file but UTF-8 without
/// NUL bytes is a text file, `text/plain` of kind `text`, which models do
/// not take and ACP agents may.
pub fn validate_upload(bytes: &[u8]) -> Result<ValidatedUpload, UploadRejection> {
    let byte_length = bytes.len() as u64;
    let Some((media_type, kind)) = recognize(Path::new(""), bytes, byte_length) else {
        return validate_text_upload(bytes);
    };
    let label = match kind {
        MediaKind::Image(_) => MediaKindLabel::IMAGE,
        MediaKind::Pdf => MediaKindLabel::PDF,
        MediaKind::Audio => MediaKindLabel::AUDIO,
        MediaKind::Video => MediaKindLabel::VIDEO,
    };
    let max_bytes = max_attachment_bytes(label);
    if byte_length > max_bytes {
        return Err(UploadRejection::TooLarge {
            kind: label,
            byte_length,
            max_bytes,
            image: None,
        });
    }
    let malformed = UploadRejection::Malformed { media_type };
    let image = match kind {
        MediaKind::Image(container) => {
            let (dimensions, within) = match container {
                ImageContainer::Png => (png_dimensions(bytes), None),
                ImageContainer::Jpeg => (jpeg_dimensions(bytes), None),
                ImageContainer::WebP => (static_webp_dimensions(bytes), None),
                ImageContainer::Gif => (
                    gif_frame_count(bytes).map(|_| gif_screen(bytes)),
                    Some(gif_within_limits(bytes)),
                ),
            };
            let (width, height) = dimensions.ok_or(malformed)?;
            if width == 0 || height == 0 {
                return Err(malformed);
            }
            if !within.unwrap_or_else(|| valid_dimensions(width, height)) {
                return Err(UploadRejection::TooLarge {
                    kind: label,
                    byte_length,
                    max_bytes,
                    image: Some((width, height)),
                });
            }
            Some((width, height))
        }
        MediaKind::Pdf => {
            if !validate_pdf(bytes) {
                return Err(malformed);
            }
            None
        }
        MediaKind::Audio | MediaKind::Video => None,
    };
    Ok(ValidatedUpload {
        media_type,
        kind: label,
        image,
    })
}

/// [`validate_upload`] of content no media type recognizes: a text file, or
/// refused.
fn validate_text_upload(bytes: &[u8]) -> Result<ValidatedUpload, UploadRejection> {
    if bytes.contains(&0) || std::str::from_utf8(bytes).is_err() {
        return Err(UploadRejection::Unrecognized);
    }
    let byte_length = bytes.len() as u64;
    if byte_length > MAX_TEXT_ATTACHMENT_BYTES {
        return Err(UploadRejection::TooLarge {
            kind: MediaKindLabel::TEXT,
            byte_length,
            max_bytes: MAX_TEXT_ATTACHMENT_BYTES,
            image: None,
        });
    }
    Ok(ValidatedUpload {
        media_type: TEXT_MEDIA_TYPE,
        kind: MediaKindLabel::TEXT,
        image: None,
    })
}

pub fn canonical_video_mime_type(value: &str) -> &str {
    match value {
        "video/mov" => "video/quicktime",
        "video/avi" => "video/x-msvideo",
        "video/mpg" => "video/mpeg",
        _ => value,
    }
}

/// The image format a file starts like.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ImageContainer {
    Png,
    Jpeg,
    WebP,
    Gif,
}

/// What a file starts like.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MediaKind {
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
pub fn validate_image(bytes: &[u8], container: ImageContainer) -> bool {
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
    let (width, height) = gif_screen(bytes);
    valid_dimensions(width, height)
        && frames <= MAX_ANIMATION_FRAMES
        && (u64::from(width) * u64::from(height))
            .checked_mul(frames as u64)
            .is_some_and(|pixels| pixels <= MAX_IMAGE_PIXELS)
}

/// The logical screen size of a GIF whose header [`gif_frame_count`] read.
fn gif_screen(bytes: &[u8]) -> (u32, u32) {
    (
        u32::from(u16::from_le_bytes([bytes[6], bytes[7]])),
        u32::from(u16::from_le_bytes([bytes[8], bytes[9]])),
    )
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
/// Structural PDF validation: a header, an `%%EOF` marker, and a
/// resolvable `startxref`; encrypted documents are refused.
pub fn validate_pdf(bytes: &[u8]) -> bool {
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

/// Whether `path` has the extension of a media format this crate recognizes.
pub fn known_media_extension(path: &Path) -> bool {
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

/// Whether `path`'s extension, when it is a media one, names `mime`.
pub fn media_extension_matches(path: &Path, mime: &str) -> bool {
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
