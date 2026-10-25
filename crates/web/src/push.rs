//! Push notifications in the browser (push notifications spec section 9):
//! what Settings › This browser offers, the label a subscription gets, the
//! `#session=` a notification opens the page at, and what the page tells
//! the daemon it shows. Pure logic; [`crate::browser`] talks to the
//! browser and the workspace to the daemon.

/// Where the page keeps the daemon URL its push subscription belongs to.
pub const PUSH_DAEMON_URL_KEY: &str = "cookie.push_daemon_url";

/// How long without input before the page says the user is not there, in
/// milliseconds.
pub const IDLE_AFTER_MS: f64 = 120_000.0;

/// What the browser says about push, as the page last read it.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct PushContext {
    /// The page is a secure context with a service worker and a
    /// `PushManager`.
    pub supported: bool,
    pub secure: bool,
    /// iOS or iPadOS, which push only to an installed app.
    pub ios: bool,
    pub standalone: bool,
    /// `Notification.permission`: `default`, `granted`, or `denied`.
    pub permission: String,
    /// The application server key of this browser's subscription, if it
    /// has one, base64url.
    pub subscribed_key: Option<String>,
}

/// What Settings › This browser › Notifications shows.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PushOffer {
    /// Not a secure context: notifications need HTTPS.
    NeedsHttps,
    /// iOS or iPadOS in the browser: add the page to the Home Screen first.
    InstallFirst,
    /// This browser has no Web Push.
    Unsupported,
    /// The user blocked notifications for the page.
    Blocked,
    /// Not subscribed: **Turn on notifications**.
    Off,
    /// Subscribed with this daemon's key.
    On,
    /// Subscribed with another daemon's key, or an older one of this
    /// daemon: **Switch to this daemon**.
    OtherDaemon,
}

impl PushContext {
    /// What to offer, given `daemon_key`, the key `notifications.get`
    /// returned.
    pub fn offer(&self, daemon_key: &str) -> PushOffer {
        if !self.secure {
            return PushOffer::NeedsHttps;
        }
        if self.ios && !self.standalone {
            return PushOffer::InstallFirst;
        }
        if !self.supported {
            return PushOffer::Unsupported;
        }
        if self.permission == "denied" {
            return PushOffer::Blocked;
        }
        match &self.subscribed_key {
            Some(key) if self.permission == "granted" && same_key(key, daemon_key) => PushOffer::On,
            Some(_) if self.permission == "granted" => PushOffer::OtherDaemon,
            _ => PushOffer::Off,
        }
    }
}

/// Whether two base64url keys are the same, padding aside.
pub fn same_key(a: &str, b: &str) -> bool {
    a.trim_end_matches('=') == b.trim_end_matches('=')
}

/// A subscription's default label: the device and the browser, from the
/// user agent and `maxTouchPoints`.
pub fn device_label(agent: &str, touch_points: i32) -> String {
    let device = if agent.contains("iPhone") {
        "iPhone"
    } else if agent.contains("iPad") || (agent.contains("Macintosh") && touch_points > 1) {
        "iPad"
    } else if agent.contains("Android") {
        if agent.contains("Mobile") {
            "Android phone"
        } else {
            "Android tablet"
        }
    } else if agent.contains("Macintosh") {
        "Mac"
    } else if agent.contains("Windows") {
        "Windows"
    } else if agent.contains("CrOS") {
        "Chromebook"
    } else if agent.contains("Linux") {
        "Linux"
    } else {
        "This device"
    };
    let browser = if agent.contains("Edg/") {
        "Edge"
    } else if agent.contains("Firefox/") || agent.contains("FxiOS/") {
        "Firefox"
    } else if agent.contains("SamsungBrowser/") {
        "Samsung Internet"
    } else if agent.contains("Chrome/") || agent.contains("CriOS/") {
        "Chrome"
    } else if agent.contains("Safari/") {
        "Safari"
    } else {
        "browser"
    };
    format!("{device} · {browser}")
}

pub use cookie_agent_client_state::presence::daemon_has_push;

/// The fragment left after taking `#session=` out: the other parameters,
/// in their order, or nothing.
pub fn fragment_without_session(fragment: &str) -> String {
    let fragment = fragment.strip_prefix('#').unwrap_or(fragment);
    let rest = url::form_urlencoded::parse(fragment.as_bytes())
        .filter(|(name, _)| name != "session")
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

/// The session a page's `location.hash` asks to open as `#session=<id>`.
pub fn session_from_fragment(fragment: &str) -> Option<String> {
    let fragment = fragment.strip_prefix('#').unwrap_or(fragment);
    url::form_urlencoded::parse(fragment.as_bytes())
        .find(|(name, _)| name == "session")
        .map(|(_, value)| value.trim().to_owned())
        .filter(|session| !session.is_empty())
}

/// What a page opened from a notification connects to: the daemon its push
/// subscription belongs to (`subscribed`), whatever it connected to last,
/// else the URL it would use anyway (`url`).
pub fn notification_daemon(url: &str, subscribed: Option<&str>) -> String {
    subscribed
        .map(str::trim)
        .filter(|subscribed| !subscribed.is_empty())
        .unwrap_or(url)
        .to_owned()
}

/// What the page tells the daemon it shows: the viewed session while the
/// page is visible, focused, and used in the last [`IDLE_AFTER_MS`].
pub fn presence<T>(viewed: Option<T>, visible: bool, focused: bool, idle_ms: f64) -> Option<T> {
    let idle = std::time::Duration::from_secs_f64(idle_ms.max(0.0) / 1000.0);
    cookie_agent_client_state::presence::shown(viewed, visible && focused, idle)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn context() -> PushContext {
        PushContext {
            supported: true,
            secure: true,
            ios: false,
            standalone: false,
            permission: "default".into(),
            subscribed_key: None,
        }
    }

    #[test]
    fn the_offer_follows_the_browser_and_the_subscription() {
        let key = "BKey";
        assert_eq!(context().offer(key), PushOffer::Off);
        assert_eq!(
            PushContext {
                secure: false,
                ..context()
            }
            .offer(key),
            PushOffer::NeedsHttps
        );
        assert_eq!(
            PushContext {
                ios: true,
                ..context()
            }
            .offer(key),
            PushOffer::InstallFirst
        );
        assert_eq!(
            PushContext {
                ios: true,
                standalone: true,
                ..context()
            }
            .offer(key),
            PushOffer::Off
        );
        assert_eq!(
            PushContext {
                supported: false,
                ..context()
            }
            .offer(key),
            PushOffer::Unsupported
        );
        assert_eq!(
            PushContext {
                permission: "denied".into(),
                ..context()
            }
            .offer(key),
            PushOffer::Blocked
        );
        let granted = PushContext {
            permission: "granted".into(),
            subscribed_key: Some("BKey==".into()),
            ..context()
        };
        assert_eq!(granted.offer(key), PushOffer::On);
        assert_eq!(granted.offer("BOther"), PushOffer::OtherDaemon);
        // A subscription the user since revoked permission for is off.
        assert_eq!(
            PushContext {
                permission: "default".into(),
                ..granted
            }
            .offer(key),
            PushOffer::Off
        );
    }

    #[test]
    fn labels_name_the_device_and_the_browser() {
        for (agent, touches, label) in [
            (
                "Mozilla/5.0 (iPhone; CPU iPhone OS 18_0 like Mac OS X) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/18.0 Mobile/15E148 Safari/604.1",
                5,
                "iPhone · Safari",
            ),
            (
                "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/18.0 Safari/605.1.15",
                5,
                "iPad · Safari",
            ),
            (
                "Mozilla/5.0 (Linux; Android 15; Pixel 9) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/140.0.0.0 Mobile Safari/537.36",
                5,
                "Android phone · Chrome",
            ),
            (
                "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/140.0.0.0 Safari/537.36 Edg/140.0.0.0",
                0,
                "Windows · Edge",
            ),
            (
                "Mozilla/5.0 (X11; Linux x86_64; rv:140.0) Gecko/20100101 Firefox/140.0",
                0,
                "Linux · Firefox",
            ),
            ("", 0, "This device · browser"),
        ] {
            assert_eq!(device_label(agent, touches), label, "{agent}");
        }
    }

    #[test]
    fn a_notification_opens_its_session_on_its_daemon() {
        assert_eq!(
            session_from_fragment("#session=01JB&x=1").as_deref(),
            Some("01JB")
        );
        assert_eq!(session_from_fragment("#token=a"), None);
        assert_eq!(fragment_without_session("#session=01JB&x=1"), "#x=1");
        assert_eq!(fragment_without_session("#session=01JB"), "");
        assert_eq!(session_from_fragment("#session="), None);
        assert_eq!(
            notification_daemon("wss://other/ws", Some("wss://pushing/ws")),
            "wss://pushing/ws"
        );
        assert_eq!(
            notification_daemon("wss://other/ws", None),
            "wss://other/ws"
        );
        assert_eq!(
            notification_daemon("wss://other/ws", Some(" ")),
            "wss://other/ws"
        );
    }

    #[test]
    fn presence_needs_a_visible_focused_used_page() {
        assert_eq!(presence(Some(1), true, true, 0.0), Some(1));
        assert_eq!(presence(Some(1), false, true, 0.0), None);
        assert_eq!(presence(Some(1), true, false, 0.0), None);
        assert_eq!(presence(Some(1), true, true, IDLE_AFTER_MS), None);
        assert_eq!(presence(None::<u8>, true, true, 0.0), None);
    }
}
