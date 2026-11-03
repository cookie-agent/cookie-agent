//! A plugin's pages: the `ui` it declares and what the daemon's listener
//! needs to serve them at `/plugins/<name>/`
//! (`docs/site/specs/plugin-web-ui.md` section 3).

use std::net::IpAddr;

use cookie_agent_protocol::{PluginState, PluginUi};

/// Where the daemon forwards a connected plugin's pages.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PluginPageTarget {
    /// The plugin's HTTP server on loopback, as `host:port`.
    pub authority: String,
    /// Decoded paths below the plugin's root that skip the access check,
    /// without a trailing `/` (except the root itself).
    public_paths: Vec<String>,
}

/// What the daemon knows of plugin pages under one name.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PluginPage {
    /// No plugin of that name, or a connected one that declared no pages.
    Unknown,
    /// A configured plugin that is not connected now.
    Unavailable(PluginState),
    /// A connected plugin's pages.
    Serving(PluginPageTarget),
}

impl PluginPageTarget {
    /// Reads a plugin's `ui` declaration, or says why the daemon cannot use
    /// it.
    pub(crate) fn from_declaration(ui: &PluginUi) -> Result<Self, String> {
        let authority = loopback_authority(&ui.url).ok_or_else(|| {
            format!(
                "its pages are not served: `ui.url` must be `http://` to a loopback address or \
                 `localhost` with a port and nothing after it, not `{}`",
                ui.url
            )
        })?;
        let public_paths = ui
            .public_paths
            .iter()
            .map(|path| {
                public_path(path).ok_or_else(|| {
                    format!(
                        "its pages are not served: public path `{path}` must start with `/` and \
                         have no query, fragment, backslash, or `.` and `..` segments"
                    )
                })
            })
            .collect::<Result<_, _>>()?;
        Ok(Self {
            authority,
            public_paths,
        })
    }

    /// Whether `path`, a request's path below the plugin's root as it came
    /// in (percent-encoded, starting with `/`), is one of the public paths
    /// or below one. A path the plugin could read as a different one (with
    /// `.` or `..` segments, encoded slashes or backslashes, or bytes that
    /// are not UTF-8) never is.
    #[must_use]
    pub fn is_public(&self, path: &str) -> bool {
        if self.public_paths.is_empty() {
            return false;
        }
        let Some(path) = decoded_path(path) else {
            return false;
        };
        self.public_paths.iter().any(|public| {
            public == "/"
                || path == *public
                || path
                    .strip_prefix(public.as_str())
                    .is_some_and(|rest| rest.starts_with('/'))
        })
    }
}

/// `host:port` of a loopback `http://` URL with nothing after the
/// authority but an optional `/`.
fn loopback_authority(url: &str) -> Option<String> {
    let rest = url.strip_prefix("http://")?;
    let authority = rest.strip_suffix('/').unwrap_or(rest);
    if authority.is_empty()
        || authority.bytes().any(|byte| {
            matches!(byte, b'/' | b'?' | b'#' | b'@' | b'\\') || byte.is_ascii_control()
        })
    {
        return None;
    }
    let (host, port) = authority.rsplit_once(':')?;
    if port.is_empty() || !port.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    port.parse::<u16>().ok().filter(|port| *port != 0)?;
    let loopback = if host.eq_ignore_ascii_case("localhost") {
        true
    } else if let Some(v6) = host
        .strip_prefix('[')
        .and_then(|host| host.strip_suffix(']'))
    {
        v6.parse::<IpAddr>().is_ok_and(|ip| ip.is_loopback())
    } else {
        host.parse::<std::net::Ipv4Addr>()
            .is_ok_and(|ip| ip.is_loopback())
    };
    loopback.then(|| authority.to_ascii_lowercase())
}

/// A declared public path, checked and without a trailing `/`.
fn public_path(path: &str) -> Option<String> {
    if !path.starts_with('/')
        || path
            .bytes()
            .any(|byte| matches!(byte, b'?' | b'#' | b'\\') || byte.is_ascii_control())
    {
        return None;
    }
    let decoded = decoded_path(path)?;
    if decoded == "/" {
        return Some(decoded);
    }
    Some(decoded.trim_end_matches('/').to_owned())
}

/// `path` with each segment percent-decoded, or `None` when a segment is
/// `.` or `..`, decodes to one holding `/` or `\`, or is not UTF-8.
fn decoded_path(path: &str) -> Option<String> {
    let mut decoded = String::with_capacity(path.len());
    for (index, segment) in path.split('/').enumerate() {
        if index > 0 {
            decoded.push('/');
        }
        let segment = percent_decode(segment)?;
        if segment == "." || segment == ".." || segment.contains(['/', '\\']) {
            return None;
        }
        decoded.push_str(&segment);
    }
    Some(decoded)
}

/// Decodes the `%XX` escapes of one path segment; `None` for a malformed
/// escape or a result that is not UTF-8.
#[must_use]
pub fn percent_decode(segment: &str) -> Option<String> {
    let bytes = segment.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' {
            let high = char::from(*bytes.get(index + 1)?).to_digit(16)?;
            let low = char::from(*bytes.get(index + 2)?).to_digit(16)?;
            decoded.push(u8::try_from(high * 16 + low).ok()?);
            index += 3;
        } else {
            decoded.push(bytes[index]);
            index += 1;
        }
    }
    String::from_utf8(decoded).ok()
}

/// Where plugin `name`'s pages are reached from outside: `page_url`
/// joined with its page path.
pub(crate) fn ui_base_url(page_url: &str, name: &str) -> String {
    format!(
        "{}/{}",
        page_url.trim_end_matches('/'),
        cookie_agent_protocol::plugin_page_path(name)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ui(url: &str, public_paths: &[&str]) -> PluginUi {
        PluginUi {
            url: url.into(),
            public_paths: public_paths.iter().map(|path| (*path).to_owned()).collect(),
        }
    }

    #[test]
    fn only_loopback_http_urls_with_a_port_are_served() {
        for (url, authority) in [
            ("http://127.0.0.1:43117", "127.0.0.1:43117"),
            ("http://127.0.0.2:8/", "127.0.0.2:8"),
            ("http://localhost:8090", "localhost:8090"),
            ("http://LocalHost:8090", "localhost:8090"),
            ("http://[::1]:9000", "[::1]:9000"),
        ] {
            let target = PluginPageTarget::from_declaration(&ui(url, &[])).unwrap();
            assert_eq!(target.authority, authority, "{url}");
        }
        for url in [
            "https://127.0.0.1:443",
            "http://10.0.0.1:80",
            "http://example.com:80",
            "http://127.0.0.1",
            "http://127.0.0.1:",
            "http://127.0.0.1:0",
            "http://127.0.0.1:70000",
            "http://user@127.0.0.1:80",
            "http://127.0.0.1:80/app",
            "http://127.0.0.1:80?x=1",
            "http://127.0.0.1:80#top",
            "http://[::2]:80",
            "http://[::1]",
            "ftp://127.0.0.1:21",
        ] {
            assert!(
                PluginPageTarget::from_declaration(&ui(url, &[])).is_err(),
                "{url} was accepted"
            );
        }
    }

    #[test]
    fn public_paths_must_be_plain_absolute_paths() {
        for path in [
            "oauth",
            "/a?b",
            "/a#b",
            "/a\\b",
            "/a/../b",
            "/./a",
            "/%2e%2e/a",
            "/a%2Fb",
        ] {
            assert!(
                PluginPageTarget::from_declaration(&ui("http://127.0.0.1:1", &[path])).is_err(),
                "{path} was accepted"
            );
        }
    }

    #[test]
    fn a_public_path_covers_itself_and_what_is_below_it() {
        let target =
            PluginPageTarget::from_declaration(&ui("http://127.0.0.1:1", &["/oauth/callback/"]))
                .unwrap();
        assert!(target.is_public("/oauth/callback"));
        assert!(target.is_public("/oauth/callback/"));
        assert!(target.is_public("/oauth/callback/google"));
        assert!(target.is_public("/oauth/%63allback"));
        assert!(!target.is_public("/oauth/callbackx"));
        assert!(!target.is_public("/oauth"));
        assert!(!target.is_public("/settings"));
    }

    #[test]
    fn paths_a_plugin_could_read_differently_are_never_public() {
        let target =
            PluginPageTarget::from_declaration(&ui("http://127.0.0.1:1", &["/oauth"])).unwrap();
        for path in [
            "/oauth/../settings",
            "/oauth/./x",
            "/oauth/%2e%2e/settings",
            "/oauth/%2E%2E/settings",
            "/oauth%2F..%2Fsettings",
            "/oauth/a%5Cb",
            "/oauth/%ff",
            "/oauth/%zz",
        ] {
            assert!(!target.is_public(path), "{path} was public");
        }
    }

    #[test]
    fn the_root_makes_every_page_public_and_no_paths_none() {
        let open = PluginPageTarget::from_declaration(&ui("http://127.0.0.1:1", &["/"])).unwrap();
        assert!(open.is_public("/"));
        assert!(open.is_public("/anything/at/all"));
        assert!(!open.is_public("/../x"));
        let closed = PluginPageTarget::from_declaration(&ui("http://127.0.0.1:1", &[])).unwrap();
        assert!(!closed.is_public("/"));
    }

    #[test]
    fn the_base_url_joins_the_page_url_and_the_encoded_name() {
        assert_eq!(
            ui_base_url("https://box.example/", "gcal"),
            "https://box.example/plugins/gcal/"
        );
        assert_eq!(
            ui_base_url("https://box.example/cookie", "my plugin"),
            "https://box.example/cookie/plugins/my%20plugin/"
        );
    }
}
