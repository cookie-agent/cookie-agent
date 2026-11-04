//! Share links for clients (`docs/site/specs/share-links.md` section 7.1):
//! whether a daemon has them, the expiries the user picks from, a link's
//! address, and whether that address looks reachable only privately.

use cookie_agent_protocol::{SemVer, Share, ShareExpiry};

/// The client protocol minor that brought share links.
const SHARES_MINOR: u32 = 86;

/// Whether a daemon of `version` keeps share links.
#[must_use]
pub fn daemon_has_shares(version: Option<SemVer>) -> bool {
    version.is_some_and(|version| {
        version
            .shared_minor(cookie_agent_protocol::CLIENT_PROTOCOL_VERSION)
            .is_some_and(|minor| minor >= SHARES_MINOR)
    })
}

/// How long a link the user makes lasts.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum ShareLifetime {
    Day,
    #[default]
    Week,
    Month,
    Never,
}

impl ShareLifetime {
    /// Every choice, in the order the dialog offers them.
    pub const ALL: [Self; 4] = [Self::Day, Self::Week, Self::Month, Self::Never];

    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Day => "1 day",
            Self::Week => "7 days",
            Self::Month => "30 days",
            Self::Never => "Never",
        }
    }

    /// The short form the CLI and the `share_link` tool take.
    #[must_use]
    pub const fn code(self) -> &'static str {
        match self {
            Self::Day => "1d",
            Self::Week => "7d",
            Self::Month => "30d",
            Self::Never => "never",
        }
    }

    #[must_use]
    pub fn from_code(code: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|lifetime| lifetime.code() == code)
    }

    /// The expiry of a link made at `now` with this lifetime.
    #[must_use]
    pub fn expiry(self, now: jiff::Timestamp) -> ShareExpiry {
        let days = match self {
            Self::Day => 1,
            Self::Week => 7,
            Self::Month => 30,
            Self::Never => return ShareExpiry::Never,
        };
        ShareExpiry::At {
            at: now + jiff::SignedDuration::from_hours(24 * days),
        }
    }
}

/// The address of `share`: its own `url`, else `page_base`, the address
/// the client reached the daemon's page at, before its `path`.
#[must_use]
pub fn share_address(share: &Share, page_base: &str) -> String {
    link_address(share.url.as_deref(), &share.path, page_base)
}

/// The address a link is copied as: its own URL, else `path` below the
/// address the client reached the daemon's page at.
#[must_use]
pub fn link_address(url: Option<&str>, path: &str, page_base: &str) -> String {
    url.map_or_else(
        || format!("{}/{path}", page_base.trim_end_matches('/')),
        str::to_owned,
    )
}

/// The address of the web page of the daemon whose client endpoint is
/// `client_url`: `ws`→`http`, `wss`→`https`, with the endpoint's final
/// `ws` dropped, so `ws://127.0.0.1:7419/ws` gives
/// `http://127.0.0.1:7419/`.
#[must_use]
pub fn page_base(client_url: &str) -> Option<String> {
    let (scheme, rest) = client_url.split_once("://")?;
    let scheme = match scheme {
        "ws" => "http",
        "wss" => "https",
        _ => return None,
    };
    let rest = rest.strip_suffix("ws").filter(|rest| rest.ends_with('/'))?;
    Some(format!("{scheme}://{rest}"))
}

/// Whether a link on `base` probably works only where the daemon's page
/// can be reached: plain `http`, a loopback, private, link-local, or
/// Tailscale (CGNAT) address, a `.local` name, or, when the daemon has no
/// share base of its own (`configured` false), a `.ts.net` name, which
/// `tailscale serve` keeps on the tailnet. It is a hint: a public address
/// cannot be told for certain.
#[must_use]
pub fn looks_private(base: &str, configured: bool) -> bool {
    let Some((scheme, rest)) = base.split_once("://") else {
        return true;
    };
    if !scheme.eq_ignore_ascii_case("https") {
        return true;
    }
    let authority = rest.split(['/', '?', '#']).next().unwrap_or_default();
    let host = authority
        .rsplit_once('@')
        .map_or(authority, |(_, host)| host);
    let host = if let Some(bracketed) = host.strip_prefix('[') {
        bracketed.split(']').next().unwrap_or_default()
    } else {
        host.rsplit_once(':').map_or(host, |(host, _)| host)
    }
    .to_ascii_lowercase();
    if let Ok(ip) = host.parse::<std::net::IpAddr>() {
        return match ip {
            std::net::IpAddr::V4(ip) => {
                let [first, second, ..] = ip.octets();
                ip.is_loopback()
                    || ip.is_private()
                    || ip.is_link_local()
                    || ip.is_unspecified()
                    || (first == 100 && (64..128).contains(&second))
            }
            std::net::IpAddr::V6(ip) => {
                let first = ip.segments()[0];
                ip.is_loopback()
                    || ip.is_unspecified()
                    || (first & 0xfe00) == 0xfc00
                    || (first & 0xffc0) == 0xfe80
            }
        };
    }
    host == "localhost"
        || host.ends_with(".localhost")
        || host.ends_with(".local")
        || host.ends_with(".internal")
        || !host.contains('.')
        || (!configured && host.ends_with(".ts.net"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shares_came_with_minor_86() {
        assert!(!daemon_has_shares(Some(SemVer::new(1, 85, 0))));
        assert!(daemon_has_shares(Some(SemVer::new(1, 86, 0))));
        assert!(!daemon_has_shares(None));
    }

    #[test]
    fn lifetimes_are_days_from_now_or_never() {
        let now: jiff::Timestamp = "2026-11-03T12:00:00Z".parse().unwrap();
        assert_eq!(
            ShareLifetime::Week.expiry(now),
            ShareExpiry::At {
                at: "2026-11-10T12:00:00Z".parse().unwrap()
            }
        );
        assert_eq!(
            ShareLifetime::Day.expiry(now),
            ShareExpiry::At {
                at: "2026-11-04T12:00:00Z".parse().unwrap()
            }
        );
        assert_eq!(ShareLifetime::Never.expiry(now), ShareExpiry::Never);
        for lifetime in ShareLifetime::ALL {
            assert_eq!(ShareLifetime::from_code(lifetime.code()), Some(lifetime));
        }
        assert_eq!(ShareLifetime::from_code("2d"), None);
    }

    #[test]
    fn the_page_base_is_the_endpoint_over_http() {
        assert_eq!(
            page_base("wss://cookie.example/sub/ws").as_deref(),
            Some("https://cookie.example/sub/")
        );
        assert_eq!(
            page_base("ws://127.0.0.1:7419/ws").as_deref(),
            Some("http://127.0.0.1:7419/")
        );
        assert_eq!(page_base("ws://127.0.0.1:7419/news"), None);
    }

    #[test]
    fn private_addresses_are_told_from_public_ones() {
        for base in [
            "http://share.example.com",
            "https://127.0.0.1:7419/",
            "https://192.168.1.4/",
            "https://10.0.0.2",
            "https://100.101.102.103:7419/",
            "https://[::1]:7419/",
            "https://[fd7a:115c:a1e0::1]/",
            "https://localhost:7419",
            "https://labserver/",
            "https://box.local/",
            "not a url",
        ] {
            assert!(looks_private(base, true), "{base}");
        }
        for base in [
            "https://share.example.com",
            "https://example.com/cookie/",
            "https://8.8.8.8/",
            "https://box.tail1234.ts.net:8443",
        ] {
            assert!(!looks_private(base, true), "{base}");
        }
        assert!(looks_private("https://box.tail1234.ts.net/", false));
    }
}
