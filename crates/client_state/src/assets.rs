//! Links into the web page for assets (`docs/site/specs/assets.md`
//! section 9): what the TUI prints under a figure.

use cookie_agent_protocol::{AssetHandle, SessionId};

/// The web page's link that opens `session` and shows its figure of the
/// asset `handle`, on the page at `page_url`. The link carries no token: a
/// browser that holds one connects, and one that does not asks for it.
#[must_use]
pub fn asset_link(page_url: &str, session: SessionId, handle: &AssetHandle) -> String {
    let page = page_url.split('#').next().unwrap_or(page_url);
    let separator = if page.ends_with('/') { "" } else { "/" };
    format!("{page}{separator}#session={session}&asset={handle}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_link_opens_the_session_and_its_asset_on_the_page() {
        let session = SessionId::new_v7();
        let handle = AssetHandle::new("asset_3f9a1c2e").unwrap();
        assert_eq!(
            asset_link("http://127.0.0.1:7419/", session, &handle),
            format!("http://127.0.0.1:7419/#session={session}&asset=asset_3f9a1c2e")
        );
        // A page URL without a trailing slash, or with a fragment, gets one
        // and loses it.
        assert_eq!(
            asset_link("https://cookie.example", session, &handle),
            format!("https://cookie.example/#session={session}&asset=asset_3f9a1c2e")
        );
        assert_eq!(
            asset_link("https://cookie.example/app/#token=x", session, &handle),
            format!("https://cookie.example/app/#session={session}&asset=asset_3f9a1c2e")
        );
    }
}
