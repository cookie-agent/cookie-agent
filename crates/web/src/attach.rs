//! Attaching files in the page: what the model accepts, as an `accept`
//! attribute, and how far an image must shrink to fit its limits before it
//! is offered downscaled.

use cookie_agent_client_state::uploads::{AcceptedMedia, RefusedLimits, too_large};
use cookie_agent_protocol::{
    InputMedia,
    media_sniff::{
        MAX_IMAGE_DIMENSION, MAX_IMAGE_PIXELS, UploadRejection, max_attachment_bytes,
        validate_upload,
    },
};

/// The `accept` attribute for a file input: every media type the model
/// takes. Empty when it takes none, so the page offers no attaching.
pub fn accept_attribute(media: &[InputMedia]) -> String {
    // Text files have no media type the browser knows them by (a `.rs` or
    // `.md` file is not `text/plain` to a file picker): offer every file,
    // and let the content decide.
    if media.iter().any(|entry| entry.kind.as_str() == "text") {
        return String::new();
    }
    let mut types = media
        .iter()
        .flat_map(|entry| entry.mime_types.iter().cloned())
        .collect::<Vec<_>>();
    types.sort();
    types.dedup();
    types.join(",")
}

/// Whether the model takes images, so the camera is offered.
pub fn accepts_images(media: &[InputMedia]) -> bool {
    media.iter().any(|entry| entry.kind.as_str() == "image")
}

/// The attach buttons in the composer's box, by what the model takes.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AttachButtons {
    /// The model takes no files.
    None,
    /// Attach a file.
    File,
    /// Attach a file, and on a phone take a photo too.
    FileAndCamera,
}

/// Which attach buttons the composer shows for a model taking `media`.
pub fn attach_buttons(media: &[InputMedia]) -> AttachButtons {
    if media.is_empty() {
        AttachButtons::None
    } else if accepts_images(media) {
        AttachButtons::FileAndCamera
    } else {
        AttachButtons::File
    }
}

/// The image limits a downscale must meet.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ImageLimits {
    pub max_dimension: Option<u32>,
    pub max_pixels: Option<u64>,
    pub max_bytes: u64,
}

/// The size an image of `width`×`height` and `bytes` bytes is re-encoded at
/// to fit `limits`: within the longest side and the pixel count, and, for
/// the byte limit, scaled by the square root of the ratio with some room,
/// since an encoder's output shrinks with the pixel count. `None` when it
/// fits as it is.
pub fn downscale_target(
    width: u32,
    height: u32,
    bytes: u64,
    limits: ImageLimits,
) -> Option<(u32, u32)> {
    let mut scale = 1.0_f64;
    if let Some(max) = limits.max_dimension {
        scale = scale.min(f64::from(max) / f64::from(width.max(height)));
    }
    if let Some(max) = limits.max_pixels {
        let pixels = f64::from(width) * f64::from(height);
        scale = scale.min((max as f64 / pixels).sqrt());
    }
    if bytes > limits.max_bytes {
        scale = scale.min((limits.max_bytes as f64 / bytes as f64).sqrt() * 0.9);
    }
    if scale >= 1.0 {
        return None;
    }
    let fit = |side: u32| ((f64::from(side) * scale).floor() as u32).max(1);
    Some((fit(width), fit(height)))
}

/// What becomes of a file the page is asked to attach.
#[derive(Clone, Debug, PartialEq)]
pub enum Assessment {
    /// It fits: upload it as it is.
    Attach,
    /// An image too large for the model or the daemon: offer to re-encode
    /// it within `limits`.
    Downscale { reason: String, limits: ImageLimits },
    /// It cannot be attached, for `0`.
    Refuse(String),
}

/// The limits an image must meet for the model and the daemon both.
pub fn image_limits(accepted: &AcceptedMedia) -> Option<ImageLimits> {
    let entry = accepted
        .media
        .iter()
        .find(|entry| entry.kind.as_str() == "image")?;
    Some(ImageLimits {
        max_dimension: Some(
            entry
                .max_dimension
                .map_or(MAX_IMAGE_DIMENSION, |side| side.min(MAX_IMAGE_DIMENSION)),
        ),
        max_pixels: Some(
            entry
                .max_pixels
                .map_or(MAX_IMAGE_PIXELS, |pixels| pixels.min(MAX_IMAGE_PIXELS)),
        ),
        max_bytes: entry.max_bytes.min(max_attachment_bytes("image")),
    })
}

/// `limits` tightened by what a daemon refusal named.
pub fn tightened(limits: ImageLimits, refused: RefusedLimits) -> ImageLimits {
    ImageLimits {
        max_dimension: match (limits.max_dimension, refused.max_dimension) {
            (Some(ours), Some(theirs)) => Some(ours.min(theirs)),
            (ours, theirs) => ours.or(theirs),
        },
        max_pixels: match (limits.max_pixels, refused.max_pixels) {
            (Some(ours), Some(theirs)) => Some(ours.min(theirs)),
            (ours, theirs) => ours.or(theirs),
        },
        max_bytes: refused
            .max_bytes
            .map_or(limits.max_bytes, |theirs| limits.max_bytes.min(theirs)),
    }
}

/// Whether `name`'s `bytes` can be attached for `accepted`, sniffed from
/// their content alone as the daemon does: as they are, after a downscale
/// (an image only; video is never re-encoded), or not at all.
pub fn assess(name: &str, bytes: &[u8], accepted: &AcceptedMedia) -> Assessment {
    if bytes.is_empty() {
        return Assessment::Refuse(format!("{name} is empty"));
    }
    let length = bytes.len() as u64;
    let downscale = |reason: String| match image_limits(accepted) {
        Some(limits) => Assessment::Downscale { reason, limits },
        None => Assessment::Refuse(reason),
    };
    match validate_upload(bytes) {
        Ok(upload) => {
            // The kind and type first: a file of a type the model does not
            // take is refused whatever its size.
            if let Err(reason) = accepted.check(name, upload.kind, upload.media_type, 0, None) {
                return Assessment::Refuse(reason);
            }
            match accepted.check(name, upload.kind, upload.media_type, length, upload.image) {
                Ok(()) => Assessment::Attach,
                Err(reason) if upload.kind == "image" => downscale(reason),
                Err(reason) => Assessment::Refuse(reason),
            }
        }
        Err(UploadRejection::Unrecognized) => Assessment::Refuse(format!(
            "{name} is not a file the model can take; {} accepts {}",
            accepted.model,
            accepted.kinds()
        )),
        Err(UploadRejection::Malformed { media_type }) => {
            Assessment::Refuse(format!("{name} starts like {media_type} but is not valid"))
        }
        Err(UploadRejection::TooLarge {
            kind,
            byte_length,
            max_bytes,
            image,
        }) => {
            let reason = match image {
                Some((width, height)) => format!(
                    "{name} is {width}×{height} pixels; images are accepted up to {MAX_IMAGE_DIMENSION} pixels a side and {MAX_IMAGE_PIXELS} pixels"
                ),
                None => too_large(name, kind, byte_length, max_bytes),
            };
            if kind == "image" {
                downscale(reason)
            } else {
                Assessment::Refuse(reason)
            }
        }
    }
}

/// The next, smaller size to try when an encode at `width`×`height` was
/// still over the byte limit.
pub fn shrink_again(width: u32, height: u32) -> (u32, u32) {
    let fit = |side: u32| ((f64::from(side) * 0.8).floor() as u32).max(1);
    (fit(width), fit(height))
}

#[cfg(test)]
mod tests {
    use cookie_agent_protocol::InputMedia;

    use cookie_agent_client_state::uploads::{AcceptedMedia, RefusedLimits};

    use super::{
        Assessment, AttachButtons, ImageLimits, accept_attribute, accepts_images, assess,
        attach_buttons, downscale_target, image_limits, shrink_again, tightened,
    };

    fn png_chunk(bytes: &mut Vec<u8>, kind: &[u8; 4], data: &[u8]) {
        bytes.extend_from_slice(&u32::try_from(data.len()).unwrap().to_be_bytes());
        bytes.extend_from_slice(kind);
        bytes.extend_from_slice(data);
        let mut crc = crc32fast::Hasher::new();
        crc.update(kind);
        crc.update(data);
        bytes.extend_from_slice(&crc.finalize().to_be_bytes());
    }

    /// A PNG of `width`×`height` whose pixel data is never decoded.
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

    fn accepted(media: Vec<InputMedia>) -> AcceptedMedia {
        AcceptedMedia {
            model: "m".into(),
            media,
        }
    }

    fn image_media(max_bytes: u64, max_dimension: Option<u32>) -> InputMedia {
        serde_json::from_value(serde_json::json!({
            "kind": "image",
            "mime_types": ["image/png", "image/jpeg"],
            "max_bytes": max_bytes,
            "max_dimension": max_dimension,
            "max_count": 4,
        }))
        .expect("input media")
    }

    fn media(kind: &str, types: &[&str]) -> InputMedia {
        serde_json::from_value(serde_json::json!({
            "kind": kind,
            "mime_types": types,
            "max_bytes": 1000,
            "max_count": 4,
        }))
        .expect("input media")
    }

    #[test]
    fn the_file_input_accepts_what_the_model_takes() {
        let image = media("image", &["image/png", "image/jpeg"]);
        let pdf = media("pdf", &["application/pdf"]);
        assert_eq!(
            accept_attribute(&[image.clone(), pdf.clone()]),
            "application/pdf,image/jpeg,image/png"
        );
        assert_eq!(accept_attribute(&[]), "");
        // An ACP agent that takes text files is offered every file.
        let text = media("text", &["text/plain"]);
        assert_eq!(accept_attribute(&[image.clone(), text.clone()]), "");
        assert_eq!(
            assess("notes.md", b"# Notes\n", &accepted(vec![text.clone()])),
            Assessment::Attach
        );
        assert!(matches!(
            assess("notes.md", b"# Notes\n", &accepted(vec![image.clone()])),
            Assessment::Refuse(reason) if reason.contains("does not accept text files")
        ));
        assert!(accepts_images(&[pdf.clone(), image.clone()]));
        assert!(!accepts_images(std::slice::from_ref(&pdf)));
        // The composer's buttons, whose room its padding keeps.
        assert_eq!(attach_buttons(&[]), AttachButtons::None);
        assert_eq!(
            attach_buttons(std::slice::from_ref(&pdf)),
            AttachButtons::File
        );
        assert_eq!(attach_buttons(&[pdf, image]), AttachButtons::FileAndCamera);
    }

    #[test]
    fn images_shrink_to_the_tightest_limit() {
        let limits = |max_dimension, max_pixels, max_bytes| ImageLimits {
            max_dimension,
            max_pixels,
            max_bytes,
        };
        // Fits: nothing to do.
        assert_eq!(
            downscale_target(800, 600, 100, limits(Some(1000), None, 1000)),
            None
        );
        // The longest side.
        assert_eq!(
            downscale_target(4000, 2000, 100, limits(Some(1000), None, 1000)),
            Some((1000, 500))
        );
        // The pixel count: 4,000,000 pixels to 1,000,000 halves each side.
        assert_eq!(
            downscale_target(2000, 2000, 100, limits(None, Some(1_000_000), 1000)),
            Some((1000, 1000))
        );
        // The byte limit: a quarter of the bytes, half of each side, with room.
        assert_eq!(
            downscale_target(1000, 1000, 4000, limits(None, None, 1000)),
            Some((450, 450))
        );
        assert_eq!(shrink_again(450, 300), (360, 240));
        assert_eq!(shrink_again(1, 1), (1, 1));
    }

    #[test]
    fn a_fitting_image_attaches_and_a_too_large_one_is_offered_a_downscale() {
        let small = png(100, 50);
        let model = accepted(vec![image_media(10_000, Some(200))]);
        assert_eq!(assess("dot.png", &small, &model), Assessment::Attach);
        let wide = png(400, 50);
        let Assessment::Downscale { reason, limits } = assess("wide.png", &wide, &model) else {
            panic!("a wide image is offered a downscale");
        };
        assert!(reason.contains("400×50"), "{reason}");
        assert_eq!(limits.max_dimension, Some(200));
        assert_eq!(limits.max_bytes, 10_000);
        // Over the byte limit: also a downscale.
        let tight = accepted(vec![image_media(10, None)]);
        assert!(matches!(
            assess("dot.png", &small, &tight),
            Assessment::Downscale { .. }
        ));
    }

    #[test]
    fn files_the_model_does_not_take_are_refused_whatever_their_size() {
        let pdf_only = accepted(vec![media("pdf", &["application/pdf"])]);
        let Assessment::Refuse(reason) = assess("dot.png", &png(100, 50), &pdf_only) else {
            panic!("an image for a pdf-only model is refused");
        };
        assert!(reason.contains("does not accept image"), "{reason}");
        assert_eq!(
            assess("empty.png", &[], &pdf_only),
            Assessment::Refuse("empty.png is empty".into())
        );
        assert!(matches!(
            assess("notes.txt", b"plain words", &pdf_only),
            Assessment::Refuse(_)
        ));
    }

    #[test]
    fn a_daemon_refusal_tightens_the_limits() {
        let model = accepted(vec![image_media(10_000, Some(4000))]);
        let limits = image_limits(&model).expect("image limits");
        let refused = RefusedLimits {
            max_bytes: Some(5_000),
            max_dimension: Some(1000),
            max_pixels: None,
        };
        let tight = tightened(limits, refused);
        assert_eq!(tight.max_bytes, 5_000);
        assert_eq!(tight.max_dimension, Some(1000));
        assert_eq!(tight.max_pixels, limits.max_pixels);
    }
}
