//! The Cookie Agent web client's built page, embedded at compile time for
//! `cookie daemon` to serve: `scripts/build-web.sh` writes it to `bundle/`
//! beside this crate. Without a built bundle the crate holds a placeholder
//! `index.html` that says how to build one.

/// One file of the page.
#[derive(Clone, Copy, Debug)]
pub struct Asset {
    /// Its path below the page's root, `/`-separated, without a leading `/`.
    pub path: &'static str,
    /// Its `Content-Type`.
    pub content_type: &'static str,
    /// Whether its name carries a hash of its content, so a client may
    /// keep it for good; `index.html` and other plain names may change.
    pub immutable: bool,
    /// A strong `ETag` of its bytes, quoted, for revalidating a file that
    /// is not immutable.
    pub etag: &'static str,
    pub bytes: &'static [u8],
    /// The file gzip-compressed, when that makes it smaller.
    pub gzip: Option<&'static [u8]>,
}

include!(concat!(env!("OUT_DIR"), "/assets.rs"));

/// The file at `path` below the page's root (`index.html` for the root).
#[must_use]
pub fn get(path: &str) -> Option<&'static Asset> {
    ASSETS.iter().find(|asset| asset.path == path)
}

/// Every file of the page.
#[must_use]
pub fn assets() -> &'static [Asset] {
    ASSETS
}

/// The page an HTML asset is shown in (`docs/site/specs/assets.md` section
/// 8.2), served with [`VIEWER_POLICY`] instead of the page's.
pub const VIEWER_PATH: &str = "viewer.html";

/// The Content-Security-Policy of [`VIEWER_PATH`]: sandboxed without
/// `allow-same-origin`, so its origin is opaque and it reaches nothing of
/// the page's, framed only by the page, and otherwise open, network
/// included: an agent's page may load scripts, styles, and data from
/// anywhere.
pub const VIEWER_POLICY: &str = "default-src * data: blob: 'unsafe-inline' 'unsafe-eval'; \
     script-src * data: blob: 'unsafe-inline' 'unsafe-eval' 'wasm-unsafe-eval'; \
     style-src * data: blob: 'unsafe-inline'; img-src * data: blob:; font-src * data: blob:; \
     media-src * data: blob:; connect-src * ws: wss: data: blob:; worker-src * data: blob:; \
     frame-src * data: blob: about:; object-src 'none'; form-action 'none'; \
     frame-ancestors 'self'; sandbox allow-scripts allow-popups";

/// The Content-Security-Policy the page is served with: its own origin,
/// its inline scripts by hash, WebAssembly, `blob:` media, WebSocket
/// connections to any daemon, and its service worker from its own origin.
#[must_use]
pub const fn content_security_policy() -> &'static str {
    POLICY
}

/// Whether the real page was built in, rather than the placeholder.
#[must_use]
pub const fn is_built() -> bool {
    BUILT
}

#[cfg(test)]
mod tests {
    #[test]
    fn the_page_always_has_an_index() {
        let index = super::get("index.html").expect("index.html");
        assert_eq!(index.content_type, "text/html; charset=utf-8");
        assert!(!index.bytes.is_empty());
        assert!(!index.immutable);
        let policy = super::content_security_policy();
        assert!(policy.contains("default-src 'self'"));
        // The service worker and the manifest, for installing the page.
        assert!(policy.contains("worker-src 'self';"), "{policy}");
        assert!(policy.contains("manifest-src 'self';"), "{policy}");
        assert!(super::get("missing.js").is_none());
    }

    #[test]
    fn the_viewer_is_sandboxed_without_its_origin_and_framed_only_by_the_page() {
        let policy = super::VIEWER_POLICY;
        assert!(
            policy.contains("sandbox allow-scripts allow-popups"),
            "{policy}"
        );
        assert!(!policy.contains("allow-same-origin"), "{policy}");
        assert!(policy.contains("frame-ancestors 'self'"), "{policy}");
        assert!(policy.contains("form-action 'none'"), "{policy}");
        // An agent's page reaches the network.
        assert!(policy.contains("connect-src * ws: wss:"), "{policy}");
    }
}
