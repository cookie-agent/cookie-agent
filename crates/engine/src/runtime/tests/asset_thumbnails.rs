//! `asset.thumbnail` (`docs/site/specs/asset-thumbnails.md`).

use std::{collections::HashSet, io::Cursor};

use base64::Engine as _;
use cookie_agent_protocol::{AssetStamp, AssetStore, AssetThumbnailParams, SessionId};

use super::support::fixture;
use crate::assets::AssetWrite;

fn write(
    engine: &crate::Engine,
    filename: &str,
    media_type: &str,
    bytes: Vec<u8>,
) -> cookie_agent_protocol::Asset {
    engine
        .inner
        .assets
        .write(
            &AssetStore::Shared,
            AssetWrite {
                replace: None,
                title: None,
                filename: Some(filename.to_owned()),
                media_type: media_type.to_owned(),
                bytes,
                stamp: AssetStamp {
                    at: jiff::Timestamp::now(),
                    session: SessionId::new_v7(),
                    agent: None,
                },
            },
            &HashSet::new(),
        )
        .unwrap()
        .0
}

#[tokio::test]
async fn a_picture_is_drawn_once_and_kept_and_a_page_has_none() {
    let fixture = fixture();
    let engine = &fixture.engine;
    let mut png = Vec::new();
    image::DynamicImage::ImageRgb8(image::RgbImage::new(1600, 1200))
        .write_to(&mut Cursor::new(&mut png), image::ImageFormat::Png)
        .unwrap();
    let picture = write(engine, "plot.png", "image/png", png);

    let ask = |asset: cookie_agent_protocol::AssetId| AssetThumbnailParams {
        store: AssetStore::Shared,
        asset,
    };
    let first = engine.asset_thumbnail(ask(picture.id)).await.unwrap();
    assert_eq!(first.sha256, picture.sha256);
    let thumbnail = first.thumbnail.clone().expect("a picture has one");
    assert_eq!(thumbnail.media_type, "image/jpeg");
    assert_eq!((thumbnail.width, thumbnail.height), (768, 576));
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(&thumbnail.data)
        .unwrap();
    assert_eq!(
        image::load_from_memory(&bytes).unwrap().width(),
        thumbnail.width
    );
    let kept = engine
        .inner
        .assets
        .dir(&AssetStore::Shared)
        .join("thumbnails")
        .join(format!("{}.jpg", picture.sha256));
    assert_eq!(std::fs::read(&kept).unwrap(), bytes);
    // Asked again, the kept one is sent.
    assert_eq!(
        engine.asset_thumbnail(ask(picture.id)).await.unwrap(),
        first
    );

    let page = write(engine, "report.html", "text/html", b"<p>hi</p>".to_vec());
    let none = engine.asset_thumbnail(ask(page.id)).await.unwrap();
    assert_eq!(none.sha256, page.sha256);
    assert_eq!(none.thumbnail, None);

    // A picture that does not decode has none, and is not drawn again.
    let broken = write(engine, "broken.png", "image/png", b"not a png".to_vec());
    assert_eq!(
        engine
            .asset_thumbnail(ask(broken.id))
            .await
            .unwrap()
            .thumbnail,
        None
    );
    assert!(engine.inner.assets.undrawable(&broken.sha256));
}
