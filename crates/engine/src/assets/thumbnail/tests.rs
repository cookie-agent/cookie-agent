use std::io::Cursor;

use cookie_agent_protocol::ASSET_THUMBNAIL_MAX_EDGE;
use image::{DynamicImage, ImageFormat, Rgb, RgbImage, Rgba, RgbaImage};

use super::{Drawer, Format, Picture, scale};

fn png(image: &DynamicImage) -> Vec<u8> {
    let mut bytes = Vec::new();
    image
        .write_to(&mut Cursor::new(&mut bytes), ImageFormat::Png)
        .unwrap();
    bytes
}

#[test]
fn a_large_picture_shrinks_to_fit_the_edge_keeping_its_shape() {
    let wide = DynamicImage::ImageRgb8(RgbImage::from_pixel(2000, 1000, Rgb([10, 20, 30])));
    let picture = scale(&png(&wide)).unwrap();
    assert_eq!(picture.format, Format::Jpeg, "opaque pictures are JPEGs");
    assert_eq!(
        (picture.width, picture.height),
        (ASSET_THUMBNAIL_MAX_EDGE, ASSET_THUMBNAIL_MAX_EDGE / 2)
    );
    assert_eq!(
        Picture::of(picture.bytes.clone(), Format::Jpeg),
        Some(picture)
    );

    let tall = DynamicImage::ImageRgb8(RgbImage::new(500, 3000));
    let picture = scale(&png(&tall)).unwrap();
    assert_eq!(picture.height, ASSET_THUMBNAIL_MAX_EDGE);
    assert!(picture.width < ASSET_THUMBNAIL_MAX_EDGE / 4);
}

#[test]
fn a_small_picture_keeps_its_size() {
    let small = DynamicImage::ImageRgb8(RgbImage::new(ASSET_THUMBNAIL_MAX_EDGE, 40));
    let picture = scale(&png(&small)).unwrap();
    assert_eq!(
        (picture.width, picture.height),
        (ASSET_THUMBNAIL_MAX_EDGE, 40)
    );
}

#[test]
fn transparency_keeps_a_png_and_an_unused_alpha_channel_does_not() {
    let mut see_through = RgbaImage::from_pixel(10, 10, Rgba([0, 0, 0, 255]));
    see_through.put_pixel(3, 3, Rgba([0, 0, 0, 0]));
    let picture = scale(&png(&DynamicImage::ImageRgba8(see_through))).unwrap();
    assert_eq!(picture.format, Format::Png);
    assert_eq!(
        Picture::of(picture.bytes.clone(), Format::Png),
        Some(picture)
    );

    let opaque = RgbaImage::from_pixel(10, 10, Rgba([0, 0, 0, 255]));
    let picture = scale(&png(&DynamicImage::ImageRgba8(opaque))).unwrap();
    assert_eq!(picture.format, Format::Jpeg);
}

#[test]
fn what_does_not_decode_has_no_thumbnail() {
    assert_eq!(scale(b"not a picture"), None);
    let mut cut = png(&DynamicImage::ImageRgb8(RgbImage::new(64, 64)));
    cut.truncate(cut.len() / 2);
    assert_eq!(scale(&cut), None);
    assert_eq!(Picture::of(b"nope".to_vec(), Format::Png), None);
}

#[test]
fn pictures_videos_and_pdfs_draw_and_nothing_else_does() {
    for media_type in ["image/png", "image/jpeg", "image/gif", "image/webp"] {
        assert_eq!(
            Drawer::of(media_type),
            Some(Drawer::Picture),
            "{media_type}"
        );
    }
    assert_eq!(Drawer::of("video/mp4"), Some(Drawer::Video));
    assert_eq!(Drawer::of("video/webm"), Some(Drawer::Video));
    assert_eq!(Drawer::of("application/pdf"), Some(Drawer::Pdf));
    for media_type in [
        "image/svg+xml",
        "text/html",
        "text/markdown",
        "audio/mpeg",
        "application/octet-stream",
    ] {
        assert_eq!(Drawer::of(media_type), None, "{media_type}");
    }
}
