//! The connect screen's decisions, independent of the browser (spec 7.2,
//! 7.3): the daemon URL a page fills in from its own address, the token a
//! `#token=` fragment carries, what a launch starts from, whether to
//! connect without a click, and the hint for a connection refused before
//! its handshake.

use url::Url;

/// The `localStorage` key of the daemon URL the user last connected to.
pub const DAEMON_URL_KEY: &str = "cookie.daemon_url";

/// The `localStorage` key of the theme preference.
pub const THEME_KEY: &str = "cookie.theme";

/// The `localStorage` key of whether a desktop window shows the sessions
/// sidebar: `hidden` once the user hid it, else shown.
pub const SIDEBAR_KEY: &str = "cookie.sidebar";

/// Whether a stored sidebar choice shows the sidebar: every value but
/// `hidden`, so the sidebar shows until the user hides it.
pub fn sidebar_shown(stored: Option<&str>) -> bool {
    stored != Some("hidden")
}

/// The `localStorage` key of the lowest diagnostic level the transcript
/// shows (`/events`).
pub const EVENT_LEVEL_KEY: &str = "cookie.event_level";

/// The `localStorage` key of how the transcript shows agent work
/// (`reduced` or `full`).
pub const TRANSCRIPT_KEY: &str = "cookie.transcript";

/// The transcript mode a stored value names; a missing or unknown one is
/// the default.
pub fn transcript_from_storage(
    stored: Option<&str>,
) -> cookie_agent_client_state::transcript_mode::TranscriptMode {
    stored
        .and_then(cookie_agent_client_state::transcript_mode::TranscriptMode::parse)
        .unwrap_or_default()
}

/// The `sessionStorage` key of this tab's client identity: each tab is its
/// own client, kept across reloads and reconnects.
pub const IDENTITY_KEY: &str = "cookie.identity";

/// The `localStorage` key of the token stored for the daemon at
/// `daemon_url`.
pub fn token_key(daemon_url: &str) -> String {
    format!("cookie.token:{daemon_url}")
}

/// The daemon URL a page at `page` (its `location.href`) fills in: the
/// page's scheme as a WebSocket scheme (`http`→`ws`, `https`→`wss`), its
/// host and port, and `ws` in the page's directory. `None` for a page that
/// is not served over HTTP, such as a `file:` page.
pub fn daemon_url_for_page(page: &str) -> Option<String> {
    let page = Url::parse(page).ok()?;
    let scheme = match page.scheme() {
        "http" => "ws",
        "https" => "wss",
        _ => return None,
    };
    let host = page.host_str()?;
    let authority = match page.port() {
        Some(port) => format!("{host}:{port}"),
        None => host.to_owned(),
    };
    let path = page.path();
    let directory = &path[..=path.rfind('/').unwrap_or(0)];
    let directory = if directory.is_empty() { "/" } else { directory };
    Some(format!("{scheme}://{authority}{directory}ws"))
}

/// The token a page's `location.hash` carries as `#token=<token>`, among
/// other `&`-separated parameters; `None` without a non-empty one.
pub fn token_from_fragment(fragment: &str) -> Option<String> {
    let fragment = fragment.strip_prefix('#').unwrap_or(fragment);
    url::form_urlencoded::parse(fragment.as_bytes())
        .find(|(name, _)| name == "token")
        .map(|(_, value)| value.trim().to_owned())
        .filter(|token| !token.is_empty())
}

/// The fragment left after taking the token out: the other parameters, in
/// their order, or nothing.
pub fn fragment_without_token(fragment: &str) -> String {
    let fragment = fragment.strip_prefix('#').unwrap_or(fragment);
    let rest = url::form_urlencoded::parse(fragment.as_bytes())
        .filter(|(name, _)| name != "token")
        .collect::<Vec<_>>();
    if rest.is_empty() {
        return String::new();
    }
    let mut serializer = url::form_urlencoded::Serializer::for_suffix(String::from("#"), 1);
    for (name, value) in &rest {
        serializer.append_pair(name, value);
    }
    serializer.finish()
}

/// What the page starts from: the daemon URL and token it fills in, and
/// what to do with a token its address carried.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Launch {
    /// The daemon URL: the one last connected to, else the page's own.
    pub url: String,
    /// The token for `url`: the fragment's, else the stored one.
    pub token: String,
    /// A token from the fragment, to store for `url`.
    pub store: Option<String>,
    /// The fragment to leave in the address bar once the token is out of
    /// it; `None` when it carried none.
    pub fragment: Option<String>,
}

/// What a page starts from (spec 7.2, 7.3), given the daemon URL it
/// `filled` in from its address, the URL it `remembered`, its `fragment`,
/// and the token `stored` for a daemon URL. An installed app starts at the
/// manifest's `start_url`, which has no token, so it connects with the
/// token stored when a `cookie web` link was opened.
pub fn launch(
    filled: &str,
    remembered: Option<&str>,
    fragment: &str,
    stored: impl Fn(&str) -> Option<String>,
) -> Launch {
    let url = remembered.unwrap_or(filled).to_owned();
    match token_from_fragment(fragment) {
        Some(token) => Launch {
            url,
            token: token.clone(),
            store: Some(token),
            fragment: Some(fragment_without_token(fragment)),
        },
        None => Launch {
            token: stored(&url).unwrap_or_default(),
            url,
            store: None,
            fragment: None,
        },
    }
}

/// The token in what the user typed or pasted into the token field: the
/// text itself, or the `#token=` of a pasted `cookie web` link, which an
/// installed app on iOS (whose storage is its own) needs once.
pub fn token_from_input(input: &str) -> String {
    let input = input.trim();
    input
        .split_once('#')
        .filter(|(before, _)| before.contains("://"))
        .and_then(|(_, fragment)| token_from_fragment(fragment))
        .unwrap_or_else(|| input.to_owned())
}

/// Whether the page connects without a click: it has a daemon URL (filled
/// in or remembered) and a token (from the fragment or stored).
pub fn connects_by_itself(daemon_url: Option<&str>, token: Option<&str>) -> bool {
    daemon_url.is_some_and(|url| !url.trim().is_empty())
        && token.is_some_and(|token| !token.trim().is_empty())
}

/// What the connect screen says when the daemon refused or closed the
/// connection before its handshake. A browser shows the page no reason, so
/// it names the usual causes.
pub fn refused_before_handshake_hint() -> String {
    "The daemon refused or closed the connection before its handshake. \
     Check the daemon URL and that the daemon is running."
        .to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_daemon_url_follows_the_page_scheme_host_and_directory() {
        for (page, daemon) in [
            ("http://127.0.0.1:7419/", "ws://127.0.0.1:7419/ws"),
            ("http://127.0.0.1:7419/index.html", "ws://127.0.0.1:7419/ws"),
            ("https://abc.efg/", "wss://abc.efg/ws"),
            ("https://abc.efg/cookie/", "wss://abc.efg/cookie/ws"),
            (
                "https://abc.efg/cookie/index.html?x=1#token=abc",
                "wss://abc.efg/cookie/ws",
            ),
            ("http://[::1]:8080/a/b/", "ws://[::1]:8080/a/b/ws"),
        ] {
            assert_eq!(daemon_url_for_page(page).as_deref(), Some(daemon), "{page}");
        }
        assert_eq!(daemon_url_for_page("file:///home/me/index.html"), None);
        assert_eq!(daemon_url_for_page("not a url"), None);
    }

    #[test]
    fn the_token_comes_from_the_fragment_and_leaves_the_rest() {
        assert_eq!(
            token_from_fragment("#token=abc_-1").as_deref(),
            Some("abc_-1")
        );
        assert_eq!(
            token_from_fragment("#view=x&token=abc").as_deref(),
            Some("abc")
        );
        assert_eq!(token_from_fragment("#token="), None);
        assert_eq!(token_from_fragment(""), None);
        assert_eq!(token_from_fragment("#other=1"), None);
        assert_eq!(fragment_without_token("#token=abc"), "");
        assert_eq!(fragment_without_token("#view=x&token=abc"), "#view=x");
    }

    #[test]
    fn an_app_launched_without_a_token_uses_the_stored_one() {
        let stored = |url: &str| (url == "wss://h.ts.net/ws").then(|| "kept".to_owned());
        // The home screen opens the manifest's start_url: no fragment.
        let launched = launch("wss://h.ts.net/ws", None, "", stored);
        assert_eq!(
            launched,
            Launch {
                url: "wss://h.ts.net/ws".into(),
                token: "kept".into(),
                store: None,
                fragment: None,
            }
        );
        assert!(connects_by_itself(
            Some(&launched.url),
            Some(&launched.token)
        ));
        // The URL last connected to wins over the page's own.
        let launched = launch("wss://other/ws", Some("wss://h.ts.net/ws"), "", stored);
        assert_eq!(launched.url, "wss://h.ts.net/ws");
        assert_eq!(launched.token, "kept");
        // A `cookie web` link's token is stored and leaves the address.
        let launched = launch("wss://h.ts.net/ws", None, "#token=new&x=1", stored);
        assert_eq!(launched.token, "new");
        assert_eq!(launched.store.as_deref(), Some("new"));
        assert_eq!(launched.fragment.as_deref(), Some("#x=1"));
        // Nothing stored and no link: the screen waits.
        let launched = launch("wss://fresh/ws", None, "", stored);
        assert!(!connects_by_itself(
            Some(&launched.url),
            Some(&launched.token)
        ));
    }

    #[test]
    fn a_pasted_link_gives_its_token() {
        assert_eq!(token_from_input(" abc "), "abc");
        assert_eq!(token_from_input("https://h.ts.net/#token=abc_-1"), "abc_-1");
        assert_eq!(
            token_from_input("http://127.0.0.1:7419/#view=x&token=t"),
            "t"
        );
        // A token is never mistaken for a link.
        assert_eq!(token_from_input("ab#cd"), "ab#cd");
        assert_eq!(token_from_input("https://h/#other=1"), "https://h/#other=1");
    }

    #[test]
    fn the_page_connects_by_itself_only_with_a_url_and_a_token() {
        assert!(connects_by_itself(Some("ws://h/ws"), Some("t")));
        assert!(!connects_by_itself(Some("ws://h/ws"), None));
        assert!(!connects_by_itself(None, Some("t")));
        assert!(!connects_by_itself(Some(" "), Some("t")));
        assert!(!connects_by_itself(Some("ws://h/ws"), Some("")));
    }

    #[test]
    fn the_sidebar_shows_unless_the_user_hid_it() {
        assert!(!sidebar_shown(Some("hidden")));
        assert!(sidebar_shown(Some("shown")));
        assert!(sidebar_shown(Some("other")));
        assert!(sidebar_shown(None));
    }

    #[test]
    fn the_refusal_hint_points_at_the_url_and_the_daemon() {
        let hint = refused_before_handshake_hint();
        assert!(hint.contains("daemon URL"), "{hint}");
        assert!(hint.contains("running"), "{hint}");
        assert!(!hint.contains("origin"), "{hint}");
        assert_eq!(token_key("ws://h/ws"), "cookie.token:ws://h/ws");
    }
}
