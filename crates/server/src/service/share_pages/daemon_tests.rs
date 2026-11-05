//! Share links through a running daemon: the landing page, the bytes, and
//! the one answer every link that does not work gets, on the main listener
//! and on the share-only one.

use std::{sync::Arc, time::Duration};

use cookie_agent_protocol::{
    AssetDeleteParams, AssetStore, ShareCreateParams, ShareExpiry, ShareRevokeParams,
    SharesListParams,
};
use tokio::{
    io::{AsyncReadExt as _, AsyncWriteExt as _},
    net::TcpStream,
};

const WAIT: Duration = Duration::from_secs(10);

const PAGE: &str = "<!doctype html><title>Loss</title><script>document.title='ran'</script>";

struct Answer {
    status: u16,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

impl Answer {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(header, _)| header == name)
            .map(|(_, value)| value.as_str())
    }

    fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }
}

/// `GET path` on `port` with `headers`, read to the end.
async fn get(port: u16, path: &str, headers: &[(&str, &str)]) -> Answer {
    let mut stream = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
    let mut request = format!("GET {path} HTTP/1.1\r\nhost: 127.0.0.1");
    for (name, value) in headers {
        request.push_str(&format!("\r\n{name}: {value}"));
    }
    request.push_str("\r\nconnection: close\r\n\r\n");
    stream.write_all(request.as_bytes()).await.unwrap();
    let mut response = Vec::new();
    tokio::time::timeout(WAIT, stream.read_to_end(&mut response))
        .await
        .expect("an answer")
        .unwrap();
    let split = response
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .expect("a header block");
    let head = String::from_utf8_lossy(&response[..split]).into_owned();
    let mut lines = head.lines();
    let status = lines
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|code| code.parse().ok())
        .expect("a status line");
    let headers = lines
        .filter_map(|line| line.split_once(':'))
        .map(|(name, value)| (name.trim().to_ascii_lowercase(), value.trim().to_owned()))
        .collect();
    Answer {
        status,
        headers,
        body: response[split + 4..].to_vec(),
    }
}

async fn serve(harness: &crate::tests::Harness) -> crate::RunningServer {
    Arc::clone(&harness.server)
        .serve_on(
            std::net::SocketAddr::from(([127, 0, 0, 1], 0)),
            crate::generate_token().unwrap(),
            None,
        )
        .await
        .unwrap()
}

fn assert_common_headers(answer: &Answer) {
    assert_eq!(answer.header("cache-control"), Some("no-store"));
    assert_eq!(answer.header("referrer-policy"), Some("no-referrer"));
    assert_eq!(answer.header("x-robots-tag"), Some("noindex, nofollow"));
    assert_eq!(answer.header("x-content-type-options"), Some("nosniff"));
}

#[tokio::test]
async fn a_link_serves_its_landing_page_and_its_bytes_without_a_token() {
    let harness = crate::tests::harness();
    let running = serve(&harness).await;
    let port = running.address().port();
    let engine = harness.engine.clone();
    let asset = tokio::task::spawn_blocking(move || {
        engine.write_asset_for_test(
            &AssetStore::Shared,
            "Loss curves",
            Some("loss.html"),
            PAGE.into(),
        )
    })
    .await
    .unwrap();
    let share = harness
        .engine
        .create_share(ShareCreateParams {
            store: AssetStore::Shared,
            asset: asset.id,
            expiry: ShareExpiry::Never,
            reuse: false,
        })
        .await
        .unwrap()
        .share;

    let landing = get(port, &format!("/{}", share.path), &[]).await;
    assert_eq!(landing.status, 200, "{}", landing.text());
    assert_common_headers(&landing);
    let policy = landing.header("content-security-policy").unwrap();
    assert!(!policy.contains("script-src"), "{policy}");
    assert!(policy.contains("default-src 'none'"), "{policy}");
    let page = landing.text();
    assert!(page.contains("Loss curves"));
    assert!(!page.contains("<script"), "the page runs no script");
    assert!(page.contains(&format!("{}/raw/loss.html", share.id)));

    // The page in its sandbox: an opaque origin, even at the top level.
    let raw = get(port, &format!("/s/{}/raw/loss.html", share.id), &[]).await;
    assert_eq!(raw.status, 200);
    assert_common_headers(&raw);
    assert_eq!(raw.body, PAGE.as_bytes());
    let policy = raw.header("content-security-policy").unwrap();
    assert!(policy.contains("sandbox allow-scripts"), "{policy}");
    assert!(!policy.contains("allow-same-origin"), "{policy}");
    assert_eq!(raw.header("content-type"), Some("text/html; charset=utf-8"));

    // A range answers with those bytes, and one past the end with none.
    let raw_path = format!("/s/{}/raw/loss.html", share.id);
    let part = get(port, &raw_path, &[("range", "bytes=2-5")]).await;
    assert_eq!(part.status, 206);
    assert_eq!(part.body, PAGE.as_bytes()[2..=5]);
    assert_eq!(
        part.header("content-range").unwrap(),
        format!("bytes 2-5/{}", PAGE.len())
    );
    let past = get(port, &raw_path, &[("range", "bytes=999999-")]).await;
    assert_eq!(past.status, 416);
    assert!(past.body.is_empty());

    let download = get(port, &format!("/s/{}/download/x", share.id), &[]).await;
    assert_eq!(download.status, 200);
    assert!(
        download
            .header("content-disposition")
            .unwrap()
            .starts_with("attachment; filename=\"loss.html\"")
    );
    assert_eq!(
        download.header("content-type"),
        Some("application/octet-stream")
    );

    let part = get(
        port,
        &format!("/s/{}/raw/loss.html", share.id),
        &[("range", "bytes=0-14")],
    )
    .await;
    assert_eq!(part.status, 206);
    assert_eq!(part.body, &PAGE.as_bytes()[..15]);
    assert_eq!(
        part.header("content-range"),
        Some(format!("bytes 0-14/{}", PAGE.len()).as_str())
    );

    // `/s/<id>/` is the landing page without its slash.
    let slash = get(port, &format!("/s/{}/", share.id), &[]).await;
    assert_eq!(slash.status, 308);
    assert_eq!(
        slash.header("location"),
        Some(format!("../{}", share.id).as_str())
    );

    // The landing page's views are counted; the bytes' are not.
    let listed = harness
        .engine
        .list_shares(SharesListParams::default())
        .await
        .unwrap()
        .shares;
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].views, 1);
    assert!(listed[0].last_viewed.is_some());

    harness.server.shutdown();
    running.wait().await;
    harness.engine.shutdown().await;
}

#[tokio::test]
async fn every_link_that_does_not_work_gets_the_same_answer() {
    let harness = crate::tests::harness();
    let running = serve(&harness).await;
    let port = running.address().port();
    let make = |title: &'static str| {
        let engine = harness.engine.clone();
        let engine_for_share = harness.engine.clone();
        async move {
            let asset = tokio::task::spawn_blocking(move || {
                engine.write_asset_for_test(
                    &AssetStore::Shared,
                    title,
                    Some("plot.png"),
                    b"\x89PNG\r\n\x1a\n not really".to_vec(),
                )
            })
            .await
            .unwrap();
            let share = engine_for_share
                .create_share(ShareCreateParams {
                    store: AssetStore::Shared,
                    asset: asset.id,
                    expiry: ShareExpiry::At {
                        at: jiff::Timestamp::now() + jiff::SignedDuration::from_hours(1),
                    },
                    reuse: false,
                })
                .await
                .unwrap()
                .share;
            (asset, share)
        }
    };
    let (_, revoked) = make("Revoked").await;
    harness
        .engine
        .revoke_share(ShareRevokeParams {
            id: revoked.id.clone(),
        })
        .await
        .unwrap();
    let (asset, orphaned) = make("Deleted").await;
    harness
        .engine
        .delete_asset(AssetDeleteParams {
            store: AssetStore::Shared,
            asset: asset.id,
        })
        .await
        .unwrap();
    let (_, live) = make("Live").await;
    assert_eq!(get(port, &format!("/{}", live.path), &[]).await.status, 200);

    let unknown = get(port, "/s/AAAAAAAAAAAAAAAAAAAAAA", &[]).await;
    assert_eq!(unknown.status, 404);
    assert_common_headers(&unknown);
    for path in [
        format!("/{}", revoked.path),
        format!("/s/{}/raw/plot.png", revoked.id),
        format!("/{}", orphaned.path),
        format!("/s/{}/download/plot.png", orphaned.id),
        "/s/not-an-id".to_owned(),
        format!("/s/{}/elsewhere", live.id),
    ] {
        let answer = get(port, &path, &[]).await;
        assert_eq!(answer.status, 404, "{path}");
        assert_eq!(answer.body, unknown.body, "{path}");
    }

    harness.server.shutdown();
    running.wait().await;
    harness.engine.shutdown().await;
}

#[tokio::test]
async fn links_answer_as_gone_while_sharing_is_off_and_none_are_made() {
    let harness = crate::tests::harness();
    let running = serve(&harness).await;
    let port = running.address().port();
    let engine = harness.engine.clone();
    let asset = tokio::task::spawn_blocking(move || {
        engine.write_asset_for_test(
            &AssetStore::Shared,
            "Notes",
            Some("notes.md"),
            b"# Hi".to_vec(),
        )
    })
    .await
    .unwrap();
    let share = harness
        .engine
        .create_share(ShareCreateParams {
            store: AssetStore::Shared,
            asset: asset.id,
            expiry: ShareExpiry::Never,
            reuse: false,
        })
        .await
        .unwrap()
        .share;
    let landing = get(port, &format!("/{}", share.path), &[]).await;
    assert_eq!(landing.status, 200);
    assert!(
        landing.text().contains("<h1>Hi</h1>"),
        "Markdown is rendered"
    );

    harness
        .engine
        .apply_config_for_test(|config| config.runtime.share.enabled = false);
    let off = get(port, &format!("/{}", share.path), &[]).await;
    assert_eq!(off.status, 404);
    assert!(
        harness
            .engine
            .create_share(ShareCreateParams {
                store: AssetStore::Shared,
                asset: asset.id,
                expiry: ShareExpiry::Never,
                reuse: false,
            })
            .await
            .is_err()
    );
    let listed = harness
        .engine
        .list_shares(SharesListParams::default())
        .await
        .unwrap();
    assert!(!listed.enabled);
    assert_eq!(
        listed.shares.len(),
        1,
        "links are kept while sharing is off"
    );

    harness.server.shutdown();
    running.wait().await;
    harness.engine.shutdown().await;
}

#[tokio::test]
async fn the_share_only_listener_serves_links_and_nothing_else() {
    let listener = crate::DaemonListener::new(cookie_agent_engine::status::StatusBoard::new(
        cookie_agent_protocol::DaemonPhase::Starting,
    ));
    let shares = listener
        .serve_shares_on(std::net::SocketAddr::from(([127, 0, 0, 1], 0)))
        .await
        .unwrap();
    let port = shares.address().port();
    let harness = crate::tests::harness();
    let engine = harness.engine.clone();
    let asset = tokio::task::spawn_blocking(move || {
        engine.write_asset_for_test(
            &AssetStore::Shared,
            "Notes",
            Some("notes.txt"),
            b"hi".to_vec(),
        )
    })
    .await
    .unwrap();
    let share = harness
        .engine
        .create_share(ShareCreateParams {
            store: AssetStore::Shared,
            asset: asset.id,
            expiry: ShareExpiry::Never,
            reuse: false,
        })
        .await
        .unwrap()
        .share;
    // Until the daemon is ready, links answer as gone.
    assert_eq!(
        get(port, &format!("/{}", share.path), &[]).await.status,
        404
    );
    listener.hand_over(Arc::clone(&harness.server));
    let landing = get(port, &format!("/{}", share.path), &[]).await;
    assert_eq!(landing.status, 200, "{}", landing.text());
    assert!(landing.text().contains("<pre class=\"text\">hi</pre>"));
    for path in [
        "/",
        "/ws",
        "/runtime",
        "/plugins/x/",
        "/index.html",
        "/viewer.html",
    ] {
        assert_eq!(get(port, path, &[]).await.status, 404, "{path}");
    }

    listener.shutdown();
    shares.wait().await;
    harness.engine.shutdown().await;
}
