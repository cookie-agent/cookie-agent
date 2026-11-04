//! Guest mode (`docs/site/specs/conversation-shares.md` section 6): the page
//! at a conversation share's `/s/<id>`. It connects to the link's own socket
//! with no token, and shows that one session as its owner sees it.

/// The socket of the conversation share whose guest page is at `page`:
/// `https://host/base/s/<id>` gives `wss://host/base/s/<id>/ws`. `None` for
/// any other page.
#[must_use]
pub fn socket_url(page: &str) -> Option<String> {
    let page = page.split(['#', '?']).next()?;
    let (scheme, rest) = page.split_once("://")?;
    let scheme = match scheme {
        "http" => "ws",
        "https" => "wss",
        _ => return None,
    };
    let id = rest.rsplit_once("/s/")?.1;
    cookie_agent_protocol::ShareId::new(id).ok()?;
    Some(format!("{scheme}://{rest}/ws"))
}

#[cfg(test)]
mod tests {
    use super::socket_url;

    const ID: &str = "q3Vt0x9LmA2bCdEfGh1iJk";

    #[test]
    fn a_guest_page_connects_to_its_own_link() {
        assert_eq!(
            socket_url(&format!("https://share.example.com/s/{ID}")).as_deref(),
            Some(format!("wss://share.example.com/s/{ID}/ws").as_str())
        );
        assert_eq!(
            socket_url(&format!("http://127.0.0.1:7419/cookie/s/{ID}#x")).as_deref(),
            Some(format!("ws://127.0.0.1:7419/cookie/s/{ID}/ws").as_str())
        );
    }

    #[test]
    fn any_other_page_is_the_owners() {
        for page in [
            "http://127.0.0.1:7419/",
            "http://127.0.0.1:7419/s/",
            "http://127.0.0.1:7419/s/short",
            "file:///tmp/index.html",
        ] {
            assert_eq!(socket_url(page), None, "{page}");
        }
        assert_eq!(socket_url(&format!("http://h/s/{ID}/raw/x.png")), None);
    }
}
