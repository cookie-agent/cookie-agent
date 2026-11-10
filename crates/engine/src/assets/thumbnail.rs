//! The thumbnails `asset.thumbnail` makes (`docs/site/specs/asset-thumbnails.md`):
//! a picture scaled down in process, a video's first frame drawn by
//! `ffmpeg`, and a PDF's first page drawn by `pdftoppm`, each found on the
//! daemon's `PATH`. The result is a JPEG, or a PNG when the picture has
//! transparent pixels, whose longer side is at most
//! [`ASSET_THUMBNAIL_MAX_EDGE`] pixels. Every other kind of content has
//! none, and clients show its icon.

use std::{ffi::OsStr, io::Cursor, path::PathBuf, process::Stdio, time::Duration};

use cookie_agent_protocol::ASSET_THUMBNAIL_MAX_EDGE;
use image::{DynamicImage, ImageFormat, ImageReader, Limits};

#[cfg(test)]
mod tests;

/// The widest and tallest picture decoded.
const MAX_SOURCE_EDGE: u32 = 16_384;
/// The most memory one decode may take.
const MAX_DECODE_BYTES: u64 = 512 * 1024 * 1024;
/// A JPEG thumbnail's quality.
const JPEG_QUALITY: u8 = 80;
/// How long `ffmpeg` or `pdftoppm` may take.
const TOOL_TIME: Duration = Duration::from_secs(30);

/// How a thumbnail is stored and sent.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Format {
    Jpeg,
    Png,
}

impl Format {
    pub(crate) const ALL: [Self; 2] = [Self::Jpeg, Self::Png];

    pub(crate) const fn extension(self) -> &'static str {
        match self {
            Self::Jpeg => "jpg",
            Self::Png => "png",
        }
    }

    pub(crate) const fn media_type(self) -> &'static str {
        match self {
            Self::Jpeg => "image/jpeg",
            Self::Png => "image/png",
        }
    }

    const fn image_format(self) -> ImageFormat {
        match self {
            Self::Jpeg => ImageFormat::Jpeg,
            Self::Png => ImageFormat::Png,
        }
    }
}

/// A thumbnail.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct Picture {
    pub(crate) format: Format,
    pub(crate) bytes: Vec<u8>,
    pub(crate) width: u32,
    pub(crate) height: u32,
}

impl Picture {
    /// A kept thumbnail, sized from its header; `None` when it does not
    /// read as `format`.
    pub(crate) fn of(bytes: Vec<u8>, format: Format) -> Option<Self> {
        let (width, height) = ImageReader::with_format(Cursor::new(&bytes), format.image_format())
            .into_dimensions()
            .ok()?;
        Some(Self {
            format,
            bytes,
            width,
            height,
        })
    }
}

/// What draws a kind of content.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Drawer {
    Picture,
    Video,
    Pdf,
}

impl Drawer {
    /// What draws content of `media_type`; `None` for a kind without
    /// thumbnails.
    pub(crate) fn of(media_type: &str) -> Option<Self> {
        match media_type {
            "image/png" | "image/jpeg" | "image/gif" | "image/webp" => Some(Self::Picture),
            "application/pdf" => Some(Self::Pdf),
            video if video.starts_with("video/") => Some(Self::Video),
            _ => None,
        }
    }
}

/// What drawing a content came to.
#[derive(Debug)]
pub(crate) enum Made {
    Picture(Picture),
    /// The content does not draw: it does not decode, or its tool refused
    /// it. Asking again gives the same.
    Cannot,
    /// The tool is not on `PATH`, did not start, or took too long; or the
    /// blob was gone. Asking again may give a thumbnail.
    Unavailable,
}

/// Draws the content at `blob` with `drawer`.
pub(crate) async fn make(blob: PathBuf, drawer: Drawer) -> Made {
    let drawn = match drawer {
        Drawer::Picture => {
            let Ok(bytes) = tokio::fs::read(&blob).await else {
                return Made::Unavailable;
            };
            bytes
        }
        Drawer::Video => {
            let edge = ASSET_THUMBNAIL_MAX_EDGE;
            let scale = format!("scale=w={edge}:h={edge}:force_original_aspect_ratio=decrease");
            let args: [&OsStr; 14] = [
                "-v".as_ref(),
                "error".as_ref(),
                "-nostdin".as_ref(),
                "-i".as_ref(),
                blob.as_os_str(),
                "-frames:v".as_ref(),
                "1".as_ref(),
                "-vf".as_ref(),
                scale.as_ref(),
                "-c:v".as_ref(),
                "png".as_ref(),
                "-f".as_ref(),
                "image2pipe".as_ref(),
                "-".as_ref(),
            ];
            match run("ffmpeg", args).await {
                Ok(png) => png,
                Err(made) => return made,
            }
        }
        Drawer::Pdf => {
            let edge = ASSET_THUMBNAIL_MAX_EDGE.to_string();
            let args: [&OsStr; 9] = [
                "-f".as_ref(),
                "1".as_ref(),
                "-l".as_ref(),
                "1".as_ref(),
                "-singlefile".as_ref(),
                "-scale-to".as_ref(),
                edge.as_ref(),
                "-png".as_ref(),
                blob.as_os_str(),
            ];
            match run("pdftoppm", args).await {
                Ok(png) => png,
                Err(made) => return made,
            }
        }
    };
    tokio::task::spawn_blocking(move || scale(&drawn).map_or(Made::Cannot, Made::Picture))
        .await
        .unwrap_or(Made::Unavailable)
}

/// What `program` writes to its output, run with `args`.
async fn run<'a>(
    program: &str,
    args: impl IntoIterator<Item = &'a OsStr>,
) -> Result<Vec<u8>, Made> {
    let child = tokio::process::Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .map_err(|_| Made::Unavailable)?;
    let output = tokio::time::timeout(TOOL_TIME, child.wait_with_output())
        .await
        .map_err(|_| Made::Unavailable)?
        .map_err(|_| Made::Unavailable)?;
    if output.status.success() && !output.stdout.is_empty() {
        Ok(output.stdout)
    } else {
        Err(Made::Cannot)
    }
}

/// The picture in `bytes`, scaled to fit [`ASSET_THUMBNAIL_MAX_EDGE`] and
/// encoded; `None` when it does not decode.
pub(crate) fn scale(bytes: &[u8]) -> Option<Picture> {
    let mut reader = ImageReader::new(Cursor::new(bytes))
        .with_guessed_format()
        .ok()?;
    let mut limits = Limits::default();
    limits.max_image_width = Some(MAX_SOURCE_EDGE);
    limits.max_image_height = Some(MAX_SOURCE_EDGE);
    limits.max_alloc = Some(MAX_DECODE_BYTES);
    reader.limits(limits);
    let image = reader.decode().ok()?;
    let edge = ASSET_THUMBNAIL_MAX_EDGE;
    let image = if image.width() > edge || image.height() > edge {
        image.thumbnail(edge, edge)
    } else {
        image
    };
    encode(&image)
}

/// `image` as a JPEG, or a PNG when a pixel of it is not opaque.
fn encode(image: &DynamicImage) -> Option<Picture> {
    let transparent =
        image.color().has_alpha() && image.to_rgba8().pixels().any(|pixel| pixel.0[3] < u8::MAX);
    let mut bytes = Vec::new();
    let format = if transparent {
        image
            .to_rgba8()
            .write_to(&mut Cursor::new(&mut bytes), ImageFormat::Png)
            .ok()?;
        Format::Png
    } else {
        image::codecs::jpeg::JpegEncoder::new_with_quality(&mut bytes, JPEG_QUALITY)
            .encode_image(&image.to_rgb8())
            .ok()?;
        Format::Jpeg
    };
    Some(Picture {
        format,
        bytes,
        width: image.width(),
        height: image.height(),
    })
}
